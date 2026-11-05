//! When digests are due: local wall-clock times in an IANA zone, turned
//! into instants one calendar day at a time, so "08:00" stays 08:00 on
//! both sides of a DST change.
//!
//! Each configured time gives at most one instant per local date:
//! - a time the clocks skip (02:30 on a spring-forward night) is due at the
//!   first minute that exists after it (03:00);
//! - a time the clocks pass twice (01:30 on a fall-back night) is due at
//!   its first pass only;
//! - two times that land on one instant (02:15 and 02:45 in a skipped
//!   hour) are one slot.

use chrono::{DateTime, Days, Duration, NaiveDate, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;

/// The longest run of local times a zone skips that is looked across
/// (real gaps are one hour, at most two).
const MAX_GAP_MINUTES: i64 = 3 * 60;

/// The instant local `time` on local `date` names in `tz` (see the module
/// doc for skipped and repeated times); `None` only for a zone that skips
/// more than [`MAX_GAP_MINUTES`] there.
pub fn instant_of(tz: &Tz, date: NaiveDate, time: NaiveTime) -> Option<DateTime<Utc>> {
    let local = date.and_time(time);
    if let Some(at) = tz.from_local_datetime(&local).earliest() {
        return Some(at.with_timezone(&Utc));
    }
    (1..=MAX_GAP_MINUTES).find_map(|m| {
        tz.from_local_datetime(&(local + Duration::minutes(m)))
            .earliest()
            .map(|at| at.with_timezone(&Utc))
    })
}

/// The slots of local `date`, in order, without repeats.
pub fn slots_on(tz: &Tz, times: &[NaiveTime], date: NaiveDate) -> Vec<DateTime<Utc>> {
    let mut slots: Vec<DateTime<Utc>> = times
        .iter()
        .filter_map(|t| instant_of(tz, date, *t))
        .collect();
    slots.sort();
    slots.dedup();
    slots
}

fn local_date(tz: &Tz, at: DateTime<Utc>) -> NaiveDate {
    at.with_timezone(tz).date_naive()
}

/// The latest slot at or before `now` (looking back two local days).
pub fn latest_at_or_before(
    tz: &Tz,
    times: &[NaiveTime],
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let today = local_date(tz, now);
    (0..=2)
        .filter_map(|back| today.checked_sub_days(Days::new(back)))
        .flat_map(|date| slots_on(tz, times, date))
        .filter(|slot| *slot <= now)
        .max()
}

/// The slot before `slot` (the span a first digest covers).
pub fn previous_before(tz: &Tz, times: &[NaiveTime], slot: DateTime<Utc>) -> Option<DateTime<Utc>> {
    latest_at_or_before(tz, times, slot - Duration::seconds(1))
}

/// The first slot after `now` (looking ahead two local days).
pub fn next_after(tz: &Tz, times: &[NaiveTime], now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let today = local_date(tz, now);
    (0..=2)
        .filter_map(|ahead| today.checked_add_days(Days::new(ahead)))
        .flat_map(|date| slots_on(tz, times, date))
        .find(|slot| *slot > now)
}

/// The next `n` slots after `now`.
pub fn upcoming(tz: &Tz, times: &[NaiveTime], now: DateTime<Utc>, n: usize) -> Vec<DateTime<Utc>> {
    let mut out = Vec::with_capacity(n);
    let mut at = now;
    while out.len() < n {
        let Some(next) = next_after(tz, times, at) else {
            break;
        };
        out.push(next);
        at = next;
    }
    out
}

/// Local midnight of `now`'s date in `tz` (the first instant of the day:
/// a zone that skips midnight starts its day at the first minute after).
pub fn day_start(tz: &Tz, now: DateTime<Utc>) -> DateTime<Utc> {
    let date = local_date(tz, now);
    instant_of(tz, date, NaiveTime::MIN).unwrap_or(now - Duration::days(1))
}

/// Local midnight of the Monday of `now`'s ISO week in `tz`.
pub fn week_start(tz: &Tz, now: DateTime<Utc>) -> DateTime<Utc> {
    use chrono::Datelike;
    let date = local_date(tz, now);
    let back = u64::from(date.weekday().num_days_from_monday());
    let monday = date.checked_sub_days(Days::new(back)).unwrap_or(date);
    instant_of(tz, monday, NaiveTime::MIN).unwrap_or(now - Duration::days(7))
}

/// What a slot at `at` (local time in `tz`) is called: `morning` before
/// noon, `afternoon` before 17:00, else `evening`.
pub fn label_of(tz: &Tz, at: DateTime<Utc>) -> &'static str {
    use chrono::Timelike;
    match at.with_timezone(tz).hour() {
        0..12 => "morning",
        12..17 => "afternoon",
        _ => "evening",
    }
}
