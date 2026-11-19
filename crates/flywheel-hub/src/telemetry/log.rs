//! The event log: every row of the telemetry store, in the order it
//! reached this hub, read by cursor and filter (`fw hub tail`, `fw hub
//! wait --fleet`, the events socket's resume, watches).
//!
//! One log, not a second store: the `events` table already is append-only,
//! keyed by an arrival-ordered row id (the forwarder's cursor), committed
//! in batches, pruned at [`super::RETENTION_DAYS`], and forwarded to a
//! collector, whose own log is therefore the fleet's. Folded into it: what
//! agents did (telemetry), label changes, and every hub lifecycle event
//! (`What::Hub`: `status.changed`, `review.*`, `queue.*`, `decision.made`,
//! ...; one about no session is filed under [`super::HUB_SCOPE`]). An
//! event that moved its session's fleet status carries that status
//! (`state`), so a state filter matches transitions, not every row.
//!
//! A cursor is opaque: this store's epoch (random, made with the store)
//! and a row id. A cursor from another store (or a wiped one) is refused,
//! never read as this one's rows.

use super::event::What;
use super::store::{Store, StoreError};
use crate::labels::Term;
use rusqlite::params;
use serde::Serialize;

/// Most rows one read returns.
pub const PAGE_MAX: usize = 1000;
/// Rows one read looks through for matches before it answers with what
/// it found (a selective filter over a busy log pages on).
pub const SCAN_MAX: usize = 20_000;

/// A position in the log: after row `at` of the store of `epoch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub epoch: u32,
    pub at: i64,
}

impl Cursor {
    pub fn encode(self) -> String {
        format!("{:08x}{:x}", self.epoch, self.at)
    }

    /// A cursor this store gave out (`epoch`: the store's).
    pub fn parse(raw: &str, epoch: u32) -> Result<Self, String> {
        let raw = raw.trim();
        let bad = || format!("cursor {raw:?} is not one this hub gave out");
        if raw.len() < 9 || !raw.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(bad());
        }
        let theirs = u32::from_str_radix(&raw[..8], 16).map_err(|_| bad())?;
        let at = i64::from_str_radix(&raw[8..], 16).map_err(|_| bad())?;
        if theirs != epoch {
            return Err(format!(
                "cursor {raw:?} is from another event log (this hub's was made again): \
                 start over without --after"
            ));
        }
        Ok(Self { epoch, at })
    }
}

/// What a read of the log selects. Empty lists select everything.
#[derive(Debug, Clone, Default)]
pub struct LogQuery {
    /// Only rows after this row id (`None`: the newest `limit`).
    pub after: Option<i64>,
    /// Only these sessions' rows (ids).
    pub sessions: Vec<String>,
    /// Only rows that moved their session to one of these statuses.
    pub states: Vec<String>,
    /// Only these kinds; a word without a dot names its family (`review`:
    /// `review.*`).
    pub kinds: Vec<String>,
    /// Only rows of sessions whose labels hold every term now, or (a
    /// `labels.changed` row) held them before or after the change.
    /// `fw.state` terms match the session's fleet status.
    pub labels: Vec<Term>,
    /// Only one hub's rows.
    pub host: Option<String>,
    /// Never these sessions' rows (a watch's owner).
    pub not_sessions: Vec<String>,
    pub limit: usize,
}

/// One row of the log.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LogRow {
    /// The cursor just after this row.
    pub cursor: String,
    #[serde(skip)]
    pub at: i64,
    pub host: String,
    pub session: String,
    pub seq: u64,
    pub ts: i64,
    pub kind: String,
    /// The status it moved its session to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub body: serde_json::Value,
    /// The session's name and workspace on this hub (filled in by the hub
    /// that owns it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

/// One read: its rows, and where the next read goes on from.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LogPage {
    pub events: Vec<LogRow>,
    /// Past every row this read looked at (or the log's end): the next
    /// read's `after`.
    pub cursor: String,
    /// More rows past `cursor` were not looked at yet.
    pub more: bool,
    /// The cursor asked for is older than anything the log still keeps.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub pruned: bool,
}

/// `kinds` as the store matches them: exact names and families.
fn kind_matches(kinds: &[String], kind: &str) -> bool {
    kinds.is_empty()
        || kinds
            .iter()
            .any(|k| kind == k || (!k.contains('.') && kind.split('.').next() == Some(k.as_str())))
}

impl Store {
    /// The log's epoch (see [`Cursor`]).
    pub fn epoch(&self) -> Result<u32, StoreError> {
        let at = self.cursor("log.epoch")?;
        Ok(u32::try_from(at).unwrap_or(1))
    }

    /// One read of the log.
    pub fn log(&self, query: &LogQuery) -> Result<LogPage, StoreError> {
        let conn = self.connection();
        let epoch = self.epoch()?;
        let head = self.head()?;
        let limit = query.limit.clamp(1, PAGE_MAX);
        let oldest: i64 = conn.query_row("SELECT COALESCE(MIN(id), 0) FROM events", [], |row| {
            row.get(0)
        })?;
        let pruned = query
            .after
            .is_some_and(|after| after > 0 && after + 1 < oldest);
        let (states, label_terms) = split_terms(&query.states, &query.labels);
        let json = |v: &[String]| serde_json::to_string(v).unwrap_or_else(|_| "[]".to_owned());
        // The narrowing SQL can do from indexes; the rest (kind families,
        // labels before a change) is checked row by row below.
        let sessions = json(&query.sessions);
        let not_sessions = json(&query.not_sessions);
        let states_json = json(&states);
        let exact_kinds: Vec<String> = query
            .kinds
            .iter()
            .filter(|k| k.contains('.'))
            .cloned()
            .collect();
        let only_exact = !query.kinds.is_empty() && exact_kinds.len() == query.kinds.len();
        let kinds_json = json(&exact_kinds);
        let newest = query.after.is_none();
        let scan = if newest {
            SCAN_MAX
        } else {
            SCAN_MAX.min(limit.max(1) * 50)
        };
        let sql = format!(
            "SELECT id, host, session, seq, ts, kind, agent, work, item, body, state FROM events
             WHERE id > ?1 AND id <= ?2
               AND (json_array_length(?3) = 0 OR session IN (SELECT value FROM json_each(?3)))
               AND session NOT IN (SELECT value FROM json_each(?4))
               AND (json_array_length(?5) = 0 OR state IN (SELECT value FROM json_each(?5)))
               AND (NOT ?6 OR kind IN (SELECT value FROM json_each(?7)))
               AND (?8 IS NULL OR host = ?8)
             ORDER BY id {} LIMIT ?9",
            if newest { "DESC" } else { "ASC" }
        );
        let after = query.after.unwrap_or(0);
        let mut statement = conn.prepare_cached(&sql)?;
        let rows = statement.query_map(
            params![
                after,
                head,
                sessions,
                not_sessions,
                states_json,
                only_exact,
                kinds_json,
                query.host,
                scan as i64
            ],
            |row| {
                let id: i64 = row.get(0)?;
                let kind: String = row.get(5)?;
                let body: String = row.get(9)?;
                Ok(LogRow {
                    cursor: Cursor { epoch, at: id }.encode(),
                    at: id,
                    host: row.get(1)?,
                    session: row.get(2)?,
                    seq: row.get::<_, i64>(3)? as u64,
                    ts: row.get(4)?,
                    kind,
                    state: row.get(10)?,
                    agent: row.get(6)?,
                    body: serde_json::from_str(&body).unwrap_or(serde_json::Value::Null),
                    name: None,
                    workspace: None,
                })
            },
        )?;
        let scanned: Vec<LogRow> = rows.filter_map(Result::ok).collect();
        let looked = scanned.len();
        let last_looked = scanned.last().map(|r| r.at);
        let mut labels = LabelCache::default();
        let mut events = Vec::new();
        let mut stopped_at = None;
        for row in scanned {
            if !kind_matches(&query.kinds, &row.kind) {
                continue;
            }
            if !label_terms.is_empty() && !labels.holds(self, &row, &label_terms)? {
                continue;
            }
            events.push(row);
            if events.len() >= limit {
                stopped_at = events.last().map(|r| r.at);
                break;
            }
        }
        if newest {
            events.reverse();
            return Ok(LogPage {
                events,
                cursor: Cursor { epoch, at: head }.encode(),
                more: false,
                pruned: false,
            });
        }
        // Where the next read goes on: past the last match when the page
        // filled, else past everything looked at (the end, when the scan
        // did not fill).
        let (next, more) = match stopped_at {
            Some(at) => (at, true),
            None if looked >= scan => (last_looked.unwrap_or(after), true),
            None => (head.max(after), false),
        };
        Ok(LogPage {
            events,
            cursor: Cursor { epoch, at: next }.encode(),
            more,
            pruned,
        })
    }

    /// The sessions with rows after `after` (at most `max` rows looked
    /// at; `None`: more than that), and whether any was a lifecycle row
    /// (the session list itself changed).
    pub fn sessions_since(
        &self,
        host: &str,
        after: i64,
        max: usize,
    ) -> Result<Option<(Vec<String>, bool)>, StoreError> {
        let conn = self.connection();
        let rows: i64 = conn.query_row(
            "SELECT COUNT(*) FROM (SELECT 1 FROM events WHERE id > ?1 LIMIT ?2)",
            params![after, max as i64 + 1],
            |row| row.get(0),
        )?;
        if rows as usize > max {
            return Ok(None);
        }
        let mut query = conn.prepare_cached(
            "SELECT session, MAX(kind LIKE 'session.%' OR kind = 'labels.changed')
             FROM events WHERE id > ?1 AND host = ?2 GROUP BY session",
        )?;
        let mut listed = false;
        let sessions = query
            .query_map(params![after, host], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
            })?
            .filter_map(Result::ok)
            .map(|(session, lifecycle)| {
                listed |= lifecycle;
                session
            })
            .filter(|s| s != super::HUB_SCOPE)
            .collect();
        Ok(Some((sessions, listed)))
    }
}

/// A query's states with its `fw.state` terms folded in, and the other
/// label terms.
fn split_terms(states: &[String], terms: &[Term]) -> (Vec<String>, Vec<Term>) {
    let mut states = states.to_vec();
    for term in terms.iter().filter(|t| t.key == crate::labels::STATE) {
        states = if states.is_empty() {
            term.values.clone()
        } else {
            states
                .into_iter()
                .filter(|s| term.values.contains(s))
                .collect()
        };
        if states.is_empty() {
            states.push(String::new());
        }
    }
    let rest = terms
        .iter()
        .filter(|t| t.key != crate::labels::STATE)
        .cloned()
        .collect();
    (states, rest)
}

/// Each session's labels, read once per log read.
#[derive(Default)]
struct LabelCache {
    labels: std::collections::HashMap<(String, String), crate::labels::Labels>,
}

impl LabelCache {
    /// Whether `row`'s session holds `terms` now, or (a label change) held
    /// them on either side of it.
    fn holds(&mut self, store: &Store, row: &LogRow, terms: &[Term]) -> Result<bool, StoreError> {
        let key = (row.host.clone(), row.session.clone());
        if !self.labels.contains_key(&key) {
            let labels = store.labels(&row.host, &row.session)?;
            self.labels.insert(key.clone(), labels);
        }
        let now = &self.labels[&key];
        if crate::labels::matches(terms, now, None) {
            return Ok(true);
        }
        let Some(What::LabelsChanged { set, before }) =
            What::from_stored(&row.kind, row.body.clone())
        else {
            return Ok(false);
        };
        let side = |change: &crate::labels::Change| {
            let mut labels = now.clone();
            for (k, v) in change {
                match v {
                    Some(v) => labels.insert(k.clone(), v.clone()),
                    None => labels.remove(k),
                };
            }
            crate::labels::matches(terms, &labels, None)
        };
        Ok(side(&set) || side(&before))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_round_trip_and_another_logs_are_refused() {
        let cursor = Cursor { epoch: 7, at: 300 };
        let raw = cursor.encode();
        assert_eq!(Cursor::parse(&raw, 7), Ok(cursor));
        assert!(
            Cursor::parse(&raw, 8)
                .unwrap_err()
                .contains("another event log")
        );
        for bad in ["", "xyz", "0000000", "zzzzzzzz1"] {
            assert!(Cursor::parse(bad, 7).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn kinds_match_by_name_or_family() {
        let kinds = vec!["review".to_owned(), "turn.finished".to_owned()];
        assert!(kind_matches(&kinds, "review.merged"));
        assert!(kind_matches(&kinds, "turn.finished"));
        assert!(!kind_matches(&kinds, "turn.started"));
        assert!(!kind_matches(&kinds, "reviewer.x"));
        assert!(kind_matches(&[], "anything"));
    }
}
