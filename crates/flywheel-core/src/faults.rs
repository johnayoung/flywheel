//! Shared fault taxonomy and bounded-backoff helper for retry sites.
//!
//! One classifier and one backoff policy that every downstream retry site
//! (agent invocation, SQLite/Postgres store, connection pool, worker loop)
//! reuses, so the TRANSIENT/PERMANENT buckets and the wait schedule never
//! fork into per-site copies.
//!
//! * [`classify_fault`] buckets an in-scope fault as
//!   [`FaultClass::Transient`] (worth retrying after a backoff) or
//!   [`FaultClass::Permanent`] (retrying cannot help), returning `None` for
//!   anything out of scope. An optimistic-concurrency version conflict is
//!   out of scope on purpose: the harness already loop-retries it, so it
//!   must never be treated as a transient fault here.
//! * [`BackoffPolicy`] / [`wait_backoff`] produce a monotonic-non-decreasing,
//!   cap-bounded wait schedule with an injectable sleep, so the waits are
//!   captured deterministically in tests instead of blocking on a clock.
//! * [`derive_session_limit_reset`] reads the agent's session-limit reset
//!   instant off a rejected rate-limit event or a refusal in the transcript.
//!
//! Lives beside the invoker seam, not in the pure `task`/`lifecycle`
//! modules.

use crate::store::{StoreConflictError, StoreError, StoreSchemaError};
use chrono::{DateTime, Days, FixedOffset, Timelike, Utc};
use flywheel_agents::RateLimitInfo;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::error::Error;
use std::fmt;
use std::sync::OnceLock;
use std::time::Duration;

/// The two retry buckets a classified fault can fall into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultClass {
    Transient,
    Permanent,
}

impl FaultClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            FaultClass::Transient => "transient",
            FaultClass::Permanent => "permanent",
        }
    }
}

impl fmt::Display for FaultClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// HTTP-status codes that mark a retryable rate-limit / overload condition,
/// matching what the invoker surfaces as `InvocationSignals::api_error_status`:
/// 429 rate-limited, 529 overloaded, plus the transient 5xx gateway set.
pub const TRANSIENT_STATUSES: [u16; 6] = [429, 500, 502, 503, 504, 529];

/// A fault handed to [`classify_fault`]: either an HTTP-ish status code (the
/// invoker's `api_error_status`) or an error value.
#[derive(Clone, Copy)]
pub enum Fault<'a> {
    Status(u16),
    Error(&'a (dyn Error + 'static)),
}

impl fmt::Debug for Fault<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fault::Status(s) => f.debug_tuple("Status").field(s).finish(),
            Fault::Error(e) => f.debug_tuple("Error").field(&e.to_string()).finish(),
        }
    }
}

impl From<u16> for Fault<'_> {
    fn from(status: u16) -> Self {
        Fault::Status(status)
    }
}

impl<'a> From<&'a (dyn Error + 'static)> for Fault<'a> {
    fn from(err: &'a (dyn Error + 'static)) -> Self {
        Fault::Error(err)
    }
}

impl<'a> From<&'a StoreError> for Fault<'a> {
    fn from(err: &'a StoreError) -> Self {
        Fault::Error(err)
    }
}

/// Bucket `fault` as TRANSIENT/PERMANENT, or `None` when out of scope.
///
/// Anything the classifier does not recognize returns `None` -- notably an
/// optimistic-concurrency version conflict, which the harness already
/// loop-retries and must never be retried here as if it were transient.
pub fn classify_fault(fault: Fault<'_>) -> Option<FaultClass> {
    match fault {
        Fault::Status(status) => classify_status(status),
        Fault::Error(err) => classify_error(err),
    }
}

/// TRANSIENT for the rate-limit / overload statuses, `None` otherwise.
pub fn classify_status(status: u16) -> Option<FaultClass> {
    TRANSIENT_STATUSES
        .contains(&status)
        .then_some(FaultClass::Transient)
}

/// Classify an error by walking it and its `source()` chain.
///
/// * PERMANENT: a schema-version mismatch ([`StoreSchemaError`]). Retrying
///   cannot reconcile an incompatible on-disk schema.
/// * Out of scope (`None`): any typed store conflict -- an
///   optimistic-concurrency conflict is already loop-retried by the harness.
/// * TRANSIENT: SQLite still busy/locked after its `busy_timeout` elapsed;
///   a dropped connection or checkout timeout surfacing as an I/O error of a
///   connection-lifecycle kind.
///
/// A wrapper that merely carries one of these (e.g. [`StoreError::Backend`]
/// around a `rusqlite::Error`) classifies as its innermost recognized cause.
pub fn classify_error(err: &(dyn Error + 'static)) -> Option<FaultClass> {
    let mut current: Option<&(dyn Error + 'static)> = Some(err);
    while let Some(err) = current {
        if let Some(verdict) = classify_one(err) {
            return verdict;
        }
        current = err.source();
    }
    None
}

/// `Some(verdict)` when `err` itself is a recognized fault (where `verdict`
/// may be `None` for a recognized-but-out-of-scope fault), `None` to keep
/// walking the source chain.
fn classify_one(err: &(dyn Error + 'static)) -> Option<Option<FaultClass>> {
    if err.downcast_ref::<StoreSchemaError>().is_some() {
        return Some(Some(FaultClass::Permanent));
    }
    if err.downcast_ref::<StoreConflictError>().is_some() {
        return Some(None);
    }
    if let Some(store) = err.downcast_ref::<StoreError>() {
        return match store {
            StoreError::Schema(_) => Some(Some(FaultClass::Permanent)),
            StoreError::Conflict(_) => Some(None),
            _ => None,
        };
    }
    if let Some(sqlite) = err.downcast_ref::<rusqlite::Error>() {
        return is_sqlite_locked(sqlite).then_some(Some(FaultClass::Transient));
    }
    if let Some(io) = err.downcast_ref::<std::io::Error>() {
        return is_connection_io_error(io).then_some(Some(FaultClass::Transient));
    }
    None
}

/// True when `err` is SQLite reporting the database busy or locked.
pub fn is_sqlite_locked(err: &rusqlite::Error) -> bool {
    use rusqlite::ErrorCode;
    match err {
        rusqlite::Error::SqliteFailure(ffi, message) => {
            matches!(
                ffi.code,
                ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked
            ) || message
                .as_deref()
                .is_some_and(|m| m.to_ascii_lowercase().contains("locked"))
        }
        other => other.to_string().to_ascii_lowercase().contains("locked"),
    }
}

/// True for the I/O error kinds a dropped connection or a checkout timeout
/// surfaces as -- the Rust-side analogue of a Postgres operational error.
pub fn is_connection_io_error(err: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    matches!(
        err.kind(),
        ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::ConnectionRefused
            | ErrorKind::NotConnected
            | ErrorKind::BrokenPipe
            | ErrorKind::TimedOut
            | ErrorKind::Interrupted
            | ErrorKind::UnexpectedEof
    )
}

pub const DEFAULT_BASE_SECONDS: f64 = 0.5;
pub const DEFAULT_FACTOR: f64 = 2.0;
pub const DEFAULT_CAP_SECONDS: f64 = 30.0;

/// An invalid [`BackoffPolicy`] parameter set.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct BackoffPolicyError(String);

/// A capped exponential backoff schedule.
///
/// `delay_for(attempt)` (0-based) grows as `base_seconds * factor^attempt`
/// until it reaches `cap_seconds` and then holds there, so the schedule is
/// monotonic-non-decreasing and every wait is `<= cap_seconds`. `factor > 1`
/// makes the pre-cap region strictly grow; a `factor == 1` policy is a
/// constant delay, not backoff.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BackoffPolicy {
    base_seconds: f64,
    factor: f64,
    cap_seconds: f64,
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        DEFAULT_BACKOFF
    }
}

/// The shared default schedule: 0.5s doubling, capped at 30s.
pub const DEFAULT_BACKOFF: BackoffPolicy = BackoffPolicy {
    base_seconds: DEFAULT_BASE_SECONDS,
    factor: DEFAULT_FACTOR,
    cap_seconds: DEFAULT_CAP_SECONDS,
};

impl BackoffPolicy {
    pub fn new(
        base_seconds: f64,
        factor: f64,
        cap_seconds: f64,
    ) -> Result<Self, BackoffPolicyError> {
        // Written to reject NaN as well as non-positive values.
        if base_seconds.is_nan() || base_seconds <= 0.0 {
            return Err(BackoffPolicyError("base_seconds must be > 0".into()));
        }
        if factor.is_nan() || factor < 1.0 {
            return Err(BackoffPolicyError("factor must be >= 1".into()));
        }
        if cap_seconds.is_nan() || cap_seconds < base_seconds {
            return Err(BackoffPolicyError(
                "cap_seconds must be >= base_seconds".into(),
            ));
        }
        Ok(Self {
            base_seconds,
            factor,
            cap_seconds,
        })
    }

    pub fn base_seconds(&self) -> f64 {
        self.base_seconds
    }

    pub fn factor(&self) -> f64 {
        self.factor
    }

    pub fn cap_seconds(&self) -> f64 {
        self.cap_seconds
    }

    /// The (capped) wait in seconds for 0-based retry `attempt`. An
    /// exponent large enough to overflow lands on the cap.
    pub fn delay_for(&self, attempt: u32) -> f64 {
        let raw = self.base_seconds * self.factor.powf(f64::from(attempt));
        if raw.is_nan() {
            return self.cap_seconds;
        }
        raw.min(self.cap_seconds)
    }

    /// [`delay_for`](Self::delay_for) as a [`Duration`].
    pub fn delay_duration_for(&self, attempt: u32) -> Duration {
        Duration::from_secs_f64(self.delay_for(attempt))
    }
}

/// Sleep for the backoff delay of 0-based retry `attempt` via the injected
/// `sleep`; return the delay in seconds.
///
/// `sleep` is injectable so tests capture the waits deterministically (push
/// into a `Vec`) instead of blocking on a real clock.
pub fn wait_backoff<F: FnMut(f64)>(attempt: u32, policy: &BackoffPolicy, mut sleep: F) -> f64 {
    let delay = policy.delay_for(attempt);
    sleep(delay);
    delay
}

/// Asynchronously sleep for the backoff delay of 0-based retry `attempt`;
/// return the delay in seconds.
pub async fn sleep_backoff(attempt: u32, policy: &BackoffPolicy) -> f64 {
    let delay = policy.delay_for(attempt);
    tokio::time::sleep(Duration::from_secs_f64(delay)).await;
    delay
}

/// Which surface answered a session-limit reset derivation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetSource {
    /// A rejected rate-limit event's `resets_at`.
    RateLimitEvent,
    /// The `...usage limit reached|<epoch>` refusal.
    RefusalPipeEpoch,
    /// A `resets 6pm` human refusal.
    RefusalClockTime,
}

impl ResetSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            ResetSource::RateLimitEvent => "rate_limit_event",
            ResetSource::RefusalPipeEpoch => "refusal_pipe_epoch",
            ResetSource::RefusalClockTime => "refusal_clock_time",
        }
    }
}

impl fmt::Display for ResetSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A derived session-limit reset instant and the ladder rung it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLimitReset {
    pub reset_at: DateTime<Utc>,
    pub source: ResetSource,
}

fn pipe_epoch_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)usage limit reached\s*\|\s*(\d+)").expect("valid regex"))
}

fn clock_time_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)resets\s+(\d{1,2})(?::(\d{2}))?\s*([ap]m)").expect("valid regex")
    })
}

/// Derive a session-limit reset instant, strongest source first.
///
/// The ladder, in order:
///
/// 1. A `rejected` rate-limit event's `resets_at_epoch` (epoch seconds,
///    UTC). Events are the serialized `flywheel_agents::RateLimitInfo`
///    values the invoker carries in `InvocationSignals::rate_limit_events`
///    (see [`rejected_rate_limit`]); an `allowed_warning` event is ignored
///    on purpose.
/// 2. A parseable refusal in `transcript`: the pipe-epoch form
///    (`...usage limit reached|<epoch>`) or the human clock-time form
///    (`resets 6pm`). The clock time is read in `now`'s offset with
///    next-occurrence semantics, so a time already past today resolves to
///    tomorrow rather than a negative interval.
/// 3. Neither derivable -> `None`.
///
/// The function is total: a malformed, garbage, or unrecognized refusal
/// yields `None`. Whether a derived reset is actually in the future is the
/// caller's policy decision -- a past epoch is returned verbatim.
pub fn derive_session_limit_reset(
    rate_limit_events: &[Value],
    transcript: &str,
    now: DateTime<FixedOffset>,
) -> Option<SessionLimitReset> {
    reset_from_rate_limit_events(rate_limit_events)
        .or_else(|| reset_from_refusal_text(transcript, now))
}

/// The normalized rate-limit info in `event` when it is a hard `rejected`
/// block, else `None`.
///
/// `event` is one entry of `InvocationSignals::rate_limit_events`: the
/// serialized `flywheel_agents::RateLimitInfo` the agents bridge emits
/// (`status`, `limit_type`, `resets_at_epoch`, native `detail`). Any other
/// shape, and every `allowed`/`allowed_warning` event, yields `None`.
#[must_use]
pub fn rejected_rate_limit(event: &Value) -> Option<RateLimitInfo> {
    RateLimitInfo::deserialize(event)
        .ok()
        .filter(|info| info.status.as_deref() == Some("rejected"))
}

fn reset_from_rate_limit_events(events: &[Value]) -> Option<SessionLimitReset> {
    events.iter().find_map(|event| {
        // Only a hard `rejected` block names a reset worth acting on; an
        // `allowed_warning` must never drive the fast-abort.
        let resets_at = rejected_rate_limit(event)?.resets_at_epoch?;
        epoch_to_utc(resets_at).map(|reset_at| SessionLimitReset {
            reset_at,
            source: ResetSource::RateLimitEvent,
        })
    })
}

fn reset_from_refusal_text(
    transcript: &str,
    now: DateTime<FixedOffset>,
) -> Option<SessionLimitReset> {
    if transcript.is_empty() {
        return None;
    }
    if let Some(reset_at) = pipe_epoch_re()
        .captures(transcript)
        .and_then(|c| c[1].parse::<f64>().ok())
        .and_then(epoch_to_utc)
    {
        return Some(SessionLimitReset {
            reset_at,
            source: ResetSource::RefusalPipeEpoch,
        });
    }
    let caps = clock_time_re().captures(transcript)?;
    let hour: u32 = caps[1].parse().ok()?;
    let minute: u32 = caps.get(2).map_or(Some(0), |m| m.as_str().parse().ok())?;
    let pm = caps[3].eq_ignore_ascii_case("pm");
    next_clock_occurrence(hour, minute, pm, now).map(|reset_at| SessionLimitReset {
        reset_at,
        source: ResetSource::RefusalClockTime,
    })
}

fn epoch_to_utc(epoch: f64) -> Option<DateTime<Utc>> {
    if !epoch.is_finite() {
        return None;
    }
    let secs = epoch.floor();
    let nanos = ((epoch - secs) * 1e9).round() as u32;
    DateTime::from_timestamp(secs as i64, nanos.min(999_999_999))
}

fn next_clock_occurrence(
    hour: u32,
    minute: u32,
    pm: bool,
    now: DateTime<FixedOffset>,
) -> Option<DateTime<Utc>> {
    if !(1..=12).contains(&hour) || minute > 59 {
        return None;
    }
    let hour24 = hour % 12 + if pm { 12 } else { 0 };
    let mut candidate = now
        .with_hour(hour24)?
        .with_minute(minute)?
        .with_second(0)?
        .with_nanosecond(0)?;
    if candidate <= now {
        candidate = candidate.checked_add_days(Days::new(1))?;
    }
    Some(candidate.with_timezone(&Utc))
}

/// Convenience for callers holding a UTC `now`.
pub fn derive_session_limit_reset_utc(
    rate_limit_events: &[Value],
    transcript: &str,
    now: DateTime<Utc>,
) -> Option<SessionLimitReset> {
    derive_session_limit_reset(rate_limit_events, transcript, now.fixed_offset())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::CURRENT_SCHEMA_VERSION;
    use chrono::TimeZone;
    use serde_json::json;

    fn locked_sqlite_error() -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some("database is locked".into()),
        )
    }

    // --- classification matrix ---------------------------------------------

    #[test]
    fn transient_statuses_classify_transient_and_others_are_out_of_scope() {
        for status in TRANSIENT_STATUSES {
            assert_eq!(
                classify_fault(Fault::Status(status)),
                Some(FaultClass::Transient)
            );
        }
        for status in [200u16, 400, 401, 403, 404, 422, 501] {
            assert_eq!(classify_fault(status.into()), None, "status {status}");
        }
    }

    #[test]
    fn sqlite_locked_is_transient() {
        let err = locked_sqlite_error();
        assert!(is_sqlite_locked(&err));
        assert_eq!(classify_error(&err), Some(FaultClass::Transient));
        let wrapped = StoreError::backend(locked_sqlite_error());
        assert_eq!(
            classify_fault((&wrapped).into()),
            Some(FaultClass::Transient)
        );
        let other = rusqlite::Error::QueryReturnedNoRows;
        assert!(!is_sqlite_locked(&other));
        assert_eq!(classify_error(&other), None);
    }

    #[test]
    fn real_sqlite_busy_from_a_concurrent_writer_is_transient() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("busy.sqlite");
        let writer = rusqlite::Connection::open(&path).unwrap();
        writer
            .execute_batch("CREATE TABLE t (x INTEGER); BEGIN EXCLUSIVE; INSERT INTO t VALUES (1);")
            .unwrap();
        let reader = rusqlite::Connection::open(&path).unwrap();
        reader.busy_timeout(Duration::ZERO).unwrap();
        let err = reader
            .execute("INSERT INTO t VALUES (2)", [])
            .expect_err("the exclusive transaction holds the lock");
        assert!(is_sqlite_locked(&err), "{err}");
        assert_eq!(classify_error(&err), Some(FaultClass::Transient));
        let as_store = StoreError::backend(err);
        assert_eq!(
            classify_fault(Fault::from(&as_store)),
            Some(FaultClass::Transient)
        );
    }

    #[test]
    fn schema_mismatch_is_permanent() {
        let schema = StoreSchemaError::new(Some(1));
        assert_eq!(schema.expected_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(classify_error(&schema), Some(FaultClass::Permanent));
        let store: StoreError = schema.into();
        assert_eq!(classify_fault((&store).into()), Some(FaultClass::Permanent));
    }

    #[test]
    fn optimistic_concurrency_conflict_is_out_of_scope() {
        let conflict = StoreError::optimistic_concurrency("run-1", 3, 4);
        assert_eq!(classify_fault((&conflict).into()), None);
        let bare = StoreConflictError::OptimisticConcurrency {
            run_id: "run-1".into(),
            expected_version: 3,
            actual_version: 4,
        };
        assert_eq!(classify_error(&bare), None);
        assert_eq!(
            classify_fault((&StoreError::lifecycle_not_found("r")).into()),
            None
        );
    }

    #[test]
    fn connection_io_errors_are_transient_and_others_are_not() {
        use std::io::{Error as IoError, ErrorKind};
        let reset = IoError::new(ErrorKind::ConnectionReset, "server closed the connection");
        assert_eq!(classify_error(&reset), Some(FaultClass::Transient));
        let timeout = StoreError::Io(IoError::new(ErrorKind::TimedOut, "pool checkout"));
        assert_eq!(
            classify_fault((&timeout).into()),
            Some(FaultClass::Transient)
        );
        let missing = IoError::new(ErrorKind::NotFound, "no such file");
        assert_eq!(classify_error(&missing), None);
        assert_eq!(classify_fault((&StoreError::Io(missing)).into()), None);
    }

    #[test]
    fn unrecognized_errors_are_out_of_scope() {
        let json_err = serde_json::from_str::<Value>("{").unwrap_err();
        assert_eq!(classify_error(&json_err), None);
        assert_eq!(classify_fault((&StoreError::Json(json_err)).into()), None);
        assert_eq!(format!("{:?}", Fault::Status(429)), "Status(429)");
    }

    // --- session-limit reset derivation ------------------------------------

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    fn now_utc() -> DateTime<FixedOffset> {
        utc(2026, 7, 9, 12, 0).fixed_offset()
    }

    /// A rejected event in the agents bridge's serialized
    /// `flywheel_agents::RateLimitInfo` shape (`resets_at` may be any JSON
    /// value so malformed resets can be exercised).
    fn rejected_event(resets_at: Value) -> Value {
        json!({
            "resets_at_epoch": resets_at,
            "status": "rejected",
            "limit_type": "five_hour",
            "detail": {"status": "rejected", "rateLimitType": "five_hour"},
        })
    }

    #[test]
    fn session_reset_from_rejected_rate_limit_event() {
        let epoch = 1_800_000_000i64;
        let result = derive_session_limit_reset(&[rejected_event(json!(epoch))], "", now_utc());
        assert_eq!(
            result,
            Some(SessionLimitReset {
                reset_at: DateTime::from_timestamp(epoch, 0).unwrap(),
                source: ResetSource::RateLimitEvent,
            })
        );
    }

    #[test]
    fn session_reset_event_precedes_transcript() {
        let result = derive_session_limit_reset(
            &[rejected_event(json!(1_800_000_000i64))],
            "Claude AI usage limit reached|1751990400",
            now_utc(),
        )
        .unwrap();
        assert_eq!(result.source, ResetSource::RateLimitEvent);
        assert_eq!(result.reset_at.timestamp(), 1_800_000_000);
    }

    #[test]
    fn session_reset_allowed_warning_event_ignored() {
        let event = json!({"status": "allowed_warning", "resets_at_epoch": 1_800_000_000i64});
        assert_eq!(derive_session_limit_reset(&[event], "", now_utc()), None);
    }

    #[test]
    fn session_reset_event_without_numeric_resets_at_falls_through() {
        for bad in [Value::Null, json!(true), json!("soon")] {
            assert_eq!(
                derive_session_limit_reset(&[rejected_event(bad)], "", now_utc()),
                None
            );
        }
        // A later well-formed event still answers.
        let events = [
            rejected_event(Value::Null),
            rejected_event(json!(1_800_000_000.5)),
        ];
        let result = derive_session_limit_reset(&events, "", now_utc()).unwrap();
        assert_eq!(result.reset_at.timestamp(), 1_800_000_000);
        assert_eq!(result.reset_at.timestamp_subsec_millis(), 500);
    }

    #[test]
    fn session_reset_from_pipe_epoch_refusal() {
        let result =
            derive_session_limit_reset(&[], "Claude AI usage limit reached|1751990400", now_utc())
                .unwrap();
        assert_eq!(result.source, ResetSource::RefusalPipeEpoch);
        assert_eq!(result.reset_at.timestamp(), 1_751_990_400);
    }

    #[test]
    fn session_reset_human_clock_time_today_when_ahead() {
        let now = utc(2026, 7, 9, 17, 0).fixed_offset();
        let result =
            derive_session_limit_reset(&[], "Claude AI usage limit reached, resets 6pm", now)
                .unwrap();
        assert_eq!(result.source, ResetSource::RefusalClockTime);
        assert_eq!(result.reset_at, utc(2026, 7, 9, 18, 0));
    }

    #[test]
    fn session_reset_human_clock_time_rolls_to_tomorrow() {
        let now = utc(2026, 7, 9, 19, 0).fixed_offset();
        let result = derive_session_limit_reset(&[], "resets 6pm", now).unwrap();
        assert_eq!(result.reset_at, utc(2026, 7, 10, 18, 0));
    }

    #[test]
    fn session_reset_human_clock_time_in_injected_offset() {
        let minus_five = FixedOffset::west_opt(5 * 3600).unwrap();
        let now = minus_five.with_ymd_and_hms(2026, 7, 9, 19, 0, 0).unwrap();
        let result = derive_session_limit_reset(&[], "resets 6pm", now).unwrap();
        // 6pm today at UTC-5 already passed (it is 7pm), so tomorrow 6pm
        // UTC-5 == 2026-07-10 23:00 UTC.
        assert_eq!(result.reset_at, utc(2026, 7, 10, 23, 0));
    }

    #[test]
    fn session_reset_human_clock_time_with_minutes_noon_and_midnight() {
        let now = utc(2026, 7, 9, 5, 0).fixed_offset();
        let result = derive_session_limit_reset(&[], "resets 6:30 am", now).unwrap();
        assert_eq!(result.reset_at, utc(2026, 7, 9, 6, 30));

        let now = utc(2026, 7, 9, 11, 0);
        let noon = derive_session_limit_reset_utc(&[], "resets 12pm", now).unwrap();
        assert_eq!(noon.reset_at, utc(2026, 7, 9, 12, 0));
        let midnight = derive_session_limit_reset_utc(&[], "resets 12am", now).unwrap();
        assert_eq!(midnight.reset_at, utc(2026, 7, 10, 0, 0));
    }

    #[test]
    fn session_reset_garbage_and_empty_inputs_are_none() {
        for transcript in [
            "the limit resets whenever it feels like it",
            "ordinary agent output with no refusal",
            "",
            "Claude AI usage limit reached|soon",
            "resets 13pm",
            "resets 6:75pm",
            "Claude AI usage limit reached|99999999999999999999",
        ] {
            assert_eq!(
                derive_session_limit_reset(&[], transcript, now_utc()),
                None,
                "{transcript:?}"
            );
        }
    }

    // --- backoff -----------------------------------------------------------

    #[test]
    fn backoff_bounded() {
        let policy = BackoffPolicy::new(0.5, 2.0, 8.0).unwrap();
        let mut recorded = Vec::new();
        let delays: Vec<f64> = (0..10)
            .map(|attempt| wait_backoff(attempt, &policy, |d| recorded.push(d)))
            .collect();
        assert_eq!(recorded, delays);
        assert!(delays.windows(2).all(|w| w[1] >= w[0]));
        assert!(delays.windows(2).any(|w| w[1] > w[0]));
        assert!(delays[1] > delays[0]);
        assert!(delays.iter().all(|d| *d <= policy.cap_seconds()));
        assert_eq!(delays[0], 0.5);
        assert_eq!(delays[4], 8.0);
        assert_eq!(delays[9], 8.0);
        assert_eq!(policy.delay_duration_for(1), Duration::from_secs(1));
    }

    #[test]
    fn backoff_overflowing_exponent_lands_on_the_cap() {
        let policy = BackoffPolicy::new(1.0, 10.0, 60.0).unwrap();
        assert_eq!(policy.delay_for(u32::MAX), 60.0);
        assert_eq!(DEFAULT_BACKOFF.delay_for(100), DEFAULT_CAP_SECONDS);
        assert_eq!(BackoffPolicy::default(), DEFAULT_BACKOFF);
        assert_eq!(DEFAULT_BACKOFF.base_seconds(), DEFAULT_BASE_SECONDS);
        assert_eq!(DEFAULT_BACKOFF.factor(), DEFAULT_FACTOR);
    }

    #[test]
    fn backoff_policy_rejects_invalid_parameters() {
        assert_eq!(
            BackoffPolicy::new(0.0, 2.0, 1.0).unwrap_err().to_string(),
            "base_seconds must be > 0"
        );
        assert_eq!(
            BackoffPolicy::new(1.0, 0.5, 1.0).unwrap_err().to_string(),
            "factor must be >= 1"
        );
        assert_eq!(
            BackoffPolicy::new(2.0, 2.0, 1.0).unwrap_err().to_string(),
            "cap_seconds must be >= base_seconds"
        );
        assert!(BackoffPolicy::new(f64::NAN, 2.0, 1.0).is_err());
        assert!(BackoffPolicy::new(1.0, 1.0, 1.0).is_ok());
    }

    #[tokio::test]
    async fn sleep_backoff_waits_the_schedule_delay() {
        let policy = BackoffPolicy::new(0.01, 2.0, 0.02).unwrap();
        let started = std::time::Instant::now();
        let delay = sleep_backoff(3, &policy).await;
        assert_eq!(delay, 0.02);
        assert!(started.elapsed() >= Duration::from_millis(20));
    }

    #[test]
    fn enums_render_their_wire_names() {
        assert_eq!(FaultClass::Transient.to_string(), "transient");
        assert_eq!(
            serde_json::to_value(FaultClass::Permanent).unwrap(),
            json!("permanent")
        );
        assert_eq!(
            ResetSource::RefusalClockTime.to_string(),
            "refusal_clock_time"
        );
        assert_eq!(
            serde_json::to_value(ResetSource::RateLimitEvent).unwrap(),
            json!("rate_limit_event")
        );
    }
}
