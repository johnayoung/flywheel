//! Scheduled digests: at configured local times (`hub.toml` `[digest]`,
//! off until `times` is set), a summary of the activity journal since the
//! previous digest -- per workspace: turns finished, agents blocked now,
//! fan-outs graded, pull requests red / green / merged, autopilot
//! decisions and gates waiting, queue stalls, spend today and this week --
//! with the top [`report::NEEDS_YOU`] "needs you" items and their links.
//!
//! Delivered over the channels configured: the ntfy-compatible push
//! ([`crate::notify`]; names and counts only), every open browser (the
//! events socket's `digest`: an in-app card and, with alerts on, a system
//! notification), and plugins (`digest.ready` with the whole digest).
//! `fw hub digest` previews one or sends it now.
//!
//! Exactly one send per slot, across DST changes, restarts, and live
//! handoffs: slots are local times turned into instants per local date
//! ([`schedule`]); the last slot decided is kept in `digest.json` under a
//! file lock ([`store`]), so the old and new hub of a handoff cannot both
//! send. A slot the hub was not running for is sent once when it starts
//! again within [`GRACE`] of it, else skipped; slots before the schedule
//! took effect (a first `times`, or a changed one) are never owed.
//! Operator doc: `docs/hub/workflows.md` (Digests).

mod report;
pub mod schedule;
mod service;
pub mod store;
#[cfg(test)]
mod tests;

pub use report::{
    Counts, Delivery, FanoutLine, Inputs, Link, Live, LiveBlocked, LiveGate, LivePr, LiveStall,
    NEEDS_YOU, Named, NeedsYou, Report, WorkspaceDigest, build, rows_from,
};
pub use service::DigestService;

use chrono::{DateTime, Duration, NaiveTime, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use store::{Marker, Pending};

/// The marker file and its lock in the state dir.
pub const STATE_FILE: &str = "digest.json";
pub const LOCK_FILE: &str = "digest.lock";
/// A slot missed (the hub was down, the machine asleep) is still sent
/// this long after it; later, it is skipped.
pub const GRACE: Duration = Duration::hours(2);
/// The longest span one digest covers (a first one after a long pause).
pub const MAX_SPAN: Duration = Duration::days(7);
/// Digests kept for the in-app card and `#digest=` links.
pub const KEEP_RECENT: usize = 14;
/// The most times a day.
pub const MAX_TIMES: usize = 8;
/// The channels, in the order they are named.
pub const CHANNELS: [&str; 3] = ["push", "browser", "plugin"];

/// `[digest]` as written in `hub.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DigestSection {
    /// Local times (`"08:00"`), at most [`MAX_TIMES`]; none: off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub times: Option<Vec<String>>,
    /// An IANA zone name (`Europe/Berlin`); default: the system's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// Any of [`CHANNELS`]; default all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channels: Option<Vec<String>>,
    /// Skip a scheduled digest with nothing in it (default true).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quiet_if_empty: Option<bool>,
}

/// Where the zone in force came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ZoneSource {
    /// `digest.timezone`.
    File,
    /// The system's zone (`TZ`, `/etc/localtime`).
    System,
    /// The system's zone is unknown: UTC.
    Utc,
}

/// The channels a digest goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Channels {
    pub push: bool,
    pub browser: bool,
    pub plugin: bool,
}

impl Default for Channels {
    fn default() -> Self {
        Self {
            push: true,
            browser: true,
            plugin: true,
        }
    }
}

impl Channels {
    pub fn names(self) -> Vec<&'static str> {
        CHANNELS
            .into_iter()
            .zip([self.push, self.browser, self.plugin])
            .filter_map(|(name, on)| on.then_some(name))
            .collect()
    }
}

/// `[digest]` resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestConfig {
    /// Sorted, without repeats; empty: off.
    pub times: Vec<NaiveTime>,
    pub timezone: Tz,
    pub zone_source: ZoneSource,
    pub channels: Channels,
    pub quiet_if_empty: bool,
}

impl Default for DigestConfig {
    fn default() -> Self {
        let (timezone, zone_source) = system_zone();
        Self {
            times: Vec::new(),
            timezone,
            zone_source,
            channels: Channels::default(),
            quiet_if_empty: true,
        }
    }
}

/// The system's zone (`TZ`, else `/etc/localtime`, as the platform names
/// it), else UTC.
pub fn system_zone() -> (Tz, ZoneSource) {
    let named = std::env::var("TZ")
        .ok()
        .map(|tz| tz.trim_start_matches(':').to_owned())
        .filter(|tz| !tz.is_empty())
        .or_else(|| iana_time_zone::get_timezone().ok());
    match named.and_then(|name| name.parse::<Tz>().ok()) {
        Some(tz) => (tz, ZoneSource::System),
        None => (Tz::UTC, ZoneSource::Utc),
    }
}

/// A local time of day: `HH:MM` (24-hour; `8:00` too).
pub fn parse_time_of_day(raw: &str) -> Result<NaiveTime, String> {
    let raw = raw.trim();
    let bad = || format!("{raw:?} is not a time of day (want HH:MM, 24-hour, e.g. \"08:00\")");
    let (h, m) = raw.split_once(':').ok_or_else(bad)?;
    if h.is_empty() || h.len() > 2 || m.len() != 2 {
        return Err(bad());
    }
    let h: u32 = h.parse().map_err(|_| bad())?;
    let m: u32 = m.parse().map_err(|_| bad())?;
    NaiveTime::from_hms_opt(h, m, 0).ok_or_else(bad)
}

impl DigestConfig {
    /// Resolve `section`; every bad value is reported (`digest.key: why`).
    pub fn resolve(section: &DigestSection) -> Result<Self, Vec<String>> {
        let mut errors = Vec::new();
        let mut config = Self::default();
        if let Some(times) = &section.times {
            if times.len() > MAX_TIMES {
                errors.push(format!(
                    "digest.times: {} times; at most {MAX_TIMES}",
                    times.len()
                ));
            }
            for raw in times {
                match parse_time_of_day(raw) {
                    Ok(t) => config.times.push(t),
                    Err(message) => errors.push(format!("digest.times: {message}")),
                }
            }
            config.times.sort();
            config.times.dedup();
        }
        if let Some(raw) = &section.timezone {
            match raw.trim().parse::<Tz>() {
                Ok(tz) => {
                    config.timezone = tz;
                    config.zone_source = ZoneSource::File;
                }
                Err(_) => errors.push(format!(
                    "digest.timezone: {raw:?} is not an IANA time zone (e.g. \"Europe/Berlin\", \"America/New_York\", \"UTC\")"
                )),
            }
        }
        if let Some(list) = &section.channels {
            let mut channels = Channels {
                push: false,
                browser: false,
                plugin: false,
            };
            for name in list {
                match name.trim() {
                    "push" => channels.push = true,
                    "browser" => channels.browser = true,
                    "plugin" => channels.plugin = true,
                    other => errors.push(format!(
                        "digest.channels: {other:?} is not a channel (push, browser, plugin)"
                    )),
                }
            }
            if list.is_empty() {
                errors.push(
                    "digest.channels: empty; name at least one of push, browser, plugin (or remove digest.times to turn digests off)"
                        .to_owned(),
                );
            }
            config.channels = channels;
        }
        if let Some(quiet) = section.quiet_if_empty {
            config.quiet_if_empty = quiet;
        }
        if errors.is_empty() {
            Ok(config)
        } else {
            Err(errors)
        }
    }

    /// Digests are scheduled (some time is set).
    pub fn enabled(&self) -> bool {
        !self.times.is_empty()
    }

    /// The times as written back (`08:00`).
    pub fn times_text(&self) -> Vec<String> {
        self.times
            .iter()
            .map(|t| t.format("%H:%M").to_string())
            .collect()
    }

    /// What the slots depend on: a different one re-arms the schedule.
    pub fn fingerprint(&self) -> String {
        format!("{}@{}", self.times_text().join(","), self.timezone.name())
    }

    /// The next `n` slots after `now`.
    pub fn upcoming(&self, now: DateTime<Utc>, n: usize) -> Vec<DateTime<Utc>> {
        schedule::upcoming(&self.timezone, &self.times, now, n)
    }
}

/// What the scheduler does now.
#[derive(Debug, Clone, PartialEq)]
pub enum Due {
    /// Digests are off.
    Off,
    /// Nothing is due (or another hub process is sending it).
    Idle,
    /// The slot was missed by more than [`GRACE`]: marked, not sent.
    Skipped(DateTime<Utc>),
    /// Send this one (claimed in the marker).
    Send(Pending),
}

/// Who is deciding: this process and this service in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claimant {
    pub pid: u32,
    pub owner: String,
}

/// Decide what is due at `now` under `config`, updating `marker` (whether
/// it changed is the second value). `alive` tells whether a pid runs.
pub fn decide(
    marker: &mut Marker,
    config: &DigestConfig,
    now: DateTime<Utc>,
    me: &Claimant,
    alive: &dyn Fn(u32) -> bool,
) -> (Due, bool) {
    if !config.enabled() {
        return (Due::Off, false);
    }
    let fingerprint = config.fingerprint();
    if marker.schedule.as_deref() != Some(fingerprint.as_str()) {
        // A new schedule: only its slots from now on are owed.
        marker.schedule = Some(fingerprint);
        marker.armed_at = Some(now);
        return (Due::Idle, true);
    }
    let mut changed = false;
    if let Some(pending) = &mut marker.pending {
        if pending.owner == me.owner && pending.pid == me.pid {
            // Ours, under way.
            return (Due::Idle, false);
        }
        if alive(pending.pid) {
            return (Due::Idle, false);
        }
        if now - pending.claimed_at <= GRACE {
            // Its claimer died mid-send: deliver it again.
            pending.pid = me.pid;
            pending.owner.clone_from(&me.owner);
            return (Due::Send(pending.clone()), true);
        }
        marker.pending = None;
        changed = true;
    }
    let tz = &config.timezone;
    let Some(due) = schedule::latest_at_or_before(tz, &config.times, now) else {
        return (Due::Idle, changed);
    };
    let floor = marker.last_slot.max(marker.armed_at);
    if floor.is_some_and(|floor| due <= floor) {
        return (Due::Idle, changed);
    }
    if now - due > GRACE {
        marker.last_slot = Some(due);
        return (Due::Skipped(due), true);
    }
    let since = marker
        .last_until
        .or_else(|| schedule::previous_before(tz, &config.times, due))
        .unwrap_or(due - Duration::days(1))
        .max(due - MAX_SPAN)
        .min(now);
    let pending = Pending {
        id: format!("dg-{}", due.timestamp()),
        slot: Some(due),
        since,
        until: now,
        pid: me.pid,
        owner: me.owner.clone(),
        claimed_at: now,
    };
    marker.last_slot = Some(due);
    marker.last_until = Some(now);
    marker.pending = Some(pending.clone());
    (Due::Send(pending), true)
}

/// Record `report` as sent: the claim (if it is this one) is released and
/// the report kept among the recent ones.
pub fn finish(marker: &mut Marker, report: &Report) {
    if marker.pending.as_ref().is_some_and(|p| p.id == report.id) {
        marker.pending = None;
    }
    marker.recent.retain(|r| r.id != report.id);
    marker.recent.push(report.clone());
    let excess = marker.recent.len().saturating_sub(KEEP_RECENT);
    marker.recent.drain(..excess);
}

/// Whether `id` is a digest id (`dg-<digits>` or `dg-m<digits>`).
pub fn valid_id(id: &str) -> bool {
    id.strip_prefix("dg-")
        .map(|rest| rest.strip_prefix('m').unwrap_or(rest))
        .is_some_and(|digits| {
            !digits.is_empty() && digits.len() <= 20 && digits.bytes().all(|b| b.is_ascii_digit())
        })
}

// --- hub.toml's [digest], written from Settings -------------------------------

/// `text` (a `hub.toml`) with its `[digest]` table replaced by `section`
/// (appended when there is none); every other line kept as it was. The
/// table runs from its header to the next table header.
pub fn splice_section(text: &str, section: &DigestSection) -> Result<String, String> {
    let body = toml::to_string(section).map_err(|e| e.to_string())?;
    let block = format!("[digest]\n{body}");
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let is_header = |line: &str| line.trim_start().starts_with('[');
    let is_digest = |line: &str| {
        let t = line.trim();
        t == "[digest]"
            || t.strip_prefix("[digest]")
                .is_some_and(|rest| rest.trim_start().starts_with('#'))
    };
    let mut out = String::with_capacity(text.len() + block.len());
    let mut i = 0;
    let mut replaced = false;
    while i < lines.len() {
        if !replaced && is_digest(lines[i]) {
            out.push_str(&block);
            i += 1;
            while i < lines.len() && !is_header(lines[i]) {
                i += 1;
            }
            // Keep a blank line before the next table.
            if i < lines.len() {
                out.push('\n');
            }
            replaced = true;
            continue;
        }
        out.push_str(lines[i]);
        i += 1;
    }
    if !replaced {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&block);
    }
    Ok(out)
}
