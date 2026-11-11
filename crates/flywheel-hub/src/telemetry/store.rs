//! The telemetry store: one SQLite file (`telemetry.db`, WAL) in the state
//! dir holding the event log and each session's folded state.
//!
//! `events` is append-only and keyed `(host, session, seq)`, so a batch
//! delivered twice changes nothing; its row id is the order events reached
//! this hub, which is the forwarder's cursor. `sessions` holds one row per
//! session: the reducer's state (to resume from), its snapshot (what the
//! fleet board lists), and the few columns the board sorts and filters by,
//! so listing ten thousand sessions never reads an event.

use super::event::{Event, What};
use super::reduce::{SessionState, Snapshot};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::path::Path;

/// The store's file in the state dir.
pub const DB_FILE: &str = "telemetry.db";
/// The schema this build reads and writes (`PRAGMA user_version`).
const SCHEMA: i64 = 2;
/// Most sessions one fleet page returns.
pub const FLEET_PAGE_MAX: usize = 1000;
/// Most events one page returns.
pub const EVENTS_PAGE_MAX: usize = 1000;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("telemetry store: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("telemetry store: the saved state of session {session} does not read: {why}")]
    State { session: String, why: String },
    #[error(
        "telemetry store {path} was written by a newer hub (schema {found}, this build knows {SCHEMA})"
    )]
    Newer { path: String, found: i64 },
}

/// What the fleet board asks for.
#[derive(Debug, Clone, Default)]
pub struct FleetQuery {
    /// Only these statuses (empty: all).
    pub statuses: Vec<String>,
    /// Only this host.
    pub host: Option<String>,
    /// Only sessions with an event at or after this (unix ms).
    pub active_since: Option<i64>,
    pub limit: usize,
    pub offset: usize,
    /// Captured runs are counted in their rollups ([`FleetPage::batches`])
    /// rather than listed one by one.
    pub rollup: bool,
    /// Only the runs of this rollup (a [`BatchRollup::key`]).
    pub batch: Option<String>,
}

/// How many sessions are in a status, and what they cost.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusCount {
    pub status: String,
    pub sessions: u64,
    pub cost_usd: f64,
}

/// One page of the fleet.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FleetPage {
    /// Most urgent first: blocked, failed, working, done, idle, ended;
    /// the latest event first within a status.
    pub sessions: Vec<Snapshot>,
    /// Sessions matching the query, all pages.
    pub total: u64,
    /// Every status's count under the same host and time filter.
    pub counts: Vec<StatusCount>,
    pub hosts: u64,
    /// With [`FleetQuery::rollup`]: the captured runs' rollups, the latest
    /// active first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub batches: Vec<BatchRollup>,
}

/// Captured runs counted together: one launcher's batch, the runs under
/// one hub session, or those from outside the hub.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BatchRollup {
    /// `batch:LABEL`, `under:SESSION`, or `outside`.
    pub key: String,
    pub host: String,
    /// The launcher's label (`batch:`), or the parent session (`under:`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// The parent's name on its hub (filled in by the hub that has it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_name: Option<String>,
    pub sessions: u64,
    /// How many are in each status, most urgent first.
    pub counts: Vec<StatusCount>,
    pub cost_usd: f64,
    /// The most any run in it was given to spend (USD).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_usd: Option<f64>,
    pub over_budget: bool,
    pub first_ts: i64,
    pub last_ts: i64,
}

/// Most rollups one fleet page returns.
pub const ROLLUPS_MAX: usize = 200;

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let found: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if found > SCHEMA {
            return Err(StoreError::Newer {
                path: path.display().to_string(),
                found,
            });
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
                 id      INTEGER PRIMARY KEY AUTOINCREMENT,
                 host    TEXT NOT NULL,
                 session TEXT NOT NULL,
                 seq     INTEGER NOT NULL,
                 ts      INTEGER NOT NULL,
                 kind    TEXT NOT NULL,
                 agent   TEXT,
                 work    TEXT,
                 item    TEXT,
                 body    TEXT NOT NULL,
                 UNIQUE (host, session, seq)
             );
             CREATE INDEX IF NOT EXISTS events_ts ON events (ts);
             CREATE TABLE IF NOT EXISTS sessions (
                 host    TEXT NOT NULL,
                 session TEXT NOT NULL,
                 status  TEXT NOT NULL,
                 rank    INTEGER NOT NULL,
                 last_ts INTEGER NOT NULL,
                 cost    REAL NOT NULL DEFAULT 0,
                 snap    TEXT NOT NULL,
                 state   TEXT NOT NULL,
                 PRIMARY KEY (host, session)
             ) WITHOUT ROWID;
             CREATE INDEX IF NOT EXISTS sessions_board ON sessions (rank, last_ts DESC);
             CREATE INDEX IF NOT EXISTS sessions_last ON sessions (last_ts);
             CREATE INDEX IF NOT EXISTS sessions_counts
                 ON sessions (last_ts, host, status, rank, cost);
             CREATE TABLE IF NOT EXISTS cursors (
                 name TEXT PRIMARY KEY,
                 at   INTEGER NOT NULL
             ) WITHOUT ROWID;",
        )?;
        // Schema 2: the rollup a captured run is counted in.
        let has_batch: bool = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('sessions') WHERE name = 'batch'",
            [],
            |row| row.get::<_, i64>(0).map(|n| n > 0),
        )?;
        if !has_batch {
            conn.execute_batch("ALTER TABLE sessions ADD COLUMN batch TEXT")?;
        }
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS sessions_batch ON sessions (batch, last_ts)",
        )?;
        conn.pragma_update(None, "user_version", SCHEMA)?;
        Ok(Self { conn })
    }

    /// Append `events` and save `states`, in one transaction.
    pub fn write(&mut self, events: &[Event], states: &[SessionState]) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        {
            let mut insert = tx.prepare_cached(
                "INSERT OR IGNORE INTO events (host, session, seq, ts, kind, agent, work, item, body)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for event in events {
                insert.execute(params![
                    event.host,
                    event.session,
                    event.seq as i64,
                    event.ts,
                    event.what.kind(),
                    event.agent,
                    event.work,
                    event.item,
                    event.what.body().to_string(),
                ])?;
            }
            let mut upsert = tx.prepare_cached(
                "INSERT INTO sessions (host, session, status, rank, last_ts, cost, snap, state, batch)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT (host, session) DO UPDATE SET
                     status = excluded.status, rank = excluded.rank, last_ts = excluded.last_ts,
                     cost = excluded.cost, snap = excluded.snap, state = excluded.state,
                     batch = excluded.batch",
            )?;
            for state in states {
                let snap = state.snapshot();
                let (snap_json, state_json) =
                    match (serde_json::to_string(&snap), serde_json::to_string(state)) {
                        (Ok(snap), Ok(state)) => (snap, state),
                        (Err(e), _) | (_, Err(e)) => {
                            eprintln!(
                                "fw hub: telemetry: session {} is not saved: {e}",
                                state.session
                            );
                            continue;
                        }
                    };
                upsert.execute(params![
                    state.host,
                    state.session,
                    snap.status,
                    rank(&snap.status),
                    snap.last_ts,
                    snap.usage.as_ref().and_then(|u| u.cost_usd).unwrap_or(0.0),
                    snap_json,
                    state_json,
                    snap.links.rollup_key(),
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// A session's saved state.
    pub fn state(&self, host: &str, session: &str) -> Result<Option<SessionState>, StoreError> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT state FROM sessions WHERE host = ?1 AND session = ?2",
                params![host, session],
                |row| row.get(0),
            )
            .optional()?;
        match raw.map(|raw| serde_json::from_str(&raw)).transpose() {
            Ok(state) => Ok(state),
            Err(e) => Err(StoreError::State {
                session: session.to_owned(),
                why: e.to_string(),
            }),
        }
    }

    /// A session's events after `after` (a `seq`), oldest first; with
    /// `newest`, the last `limit` of them instead.
    pub fn events(
        &self,
        host: &str,
        session: &str,
        after: u64,
        limit: usize,
        newest: bool,
    ) -> Result<Vec<Event>, StoreError> {
        let limit = limit.clamp(1, EVENTS_PAGE_MAX) as i64;
        let order = if newest { "DESC" } else { "ASC" };
        let mut query = self.conn.prepare_cached(&format!(
            "SELECT host, session, seq, ts, kind, agent, work, item, body FROM events
             WHERE host = ?1 AND session = ?2 AND seq > ?3 ORDER BY seq {order} LIMIT ?4"
        ))?;
        let mut events: Vec<Event> = query
            .query_map(params![host, session, after as i64, limit], read_event)?
            .filter_map(|row| row.ok().flatten())
            .collect();
        if newest {
            events.reverse();
        }
        Ok(events)
    }

    /// Events `host` observed that reached this store after row `after`,
    /// in that order, with the row each is at (the forwarder's cursor).
    pub fn outbox(
        &self,
        host: &str,
        after: i64,
        limit: usize,
    ) -> Result<Vec<(i64, Event)>, StoreError> {
        let mut query = self.conn.prepare_cached(
            "SELECT id, host, session, seq, ts, kind, agent, work, item, body FROM events
             WHERE id > ?1 AND host = ?2 ORDER BY id LIMIT ?3",
        )?;
        let rows = query.query_map(params![after, host, limit as i64], |row| {
            let id: i64 = row.get(0)?;
            let event = read_event_at(row, 1)?;
            Ok(event.map(|event| (id, event)))
        })?;
        Ok(rows.filter_map(|row| row.ok().flatten()).collect())
    }

    pub fn cursor(&self, name: &str) -> Result<i64, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT at FROM cursors WHERE name = ?1",
                params![name],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    pub fn set_cursor(&mut self, name: &str, at: i64) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO cursors (name, at) VALUES (?1, ?2)
             ON CONFLICT (name) DO UPDATE SET at = excluded.at",
            params![name, at],
        )?;
        Ok(())
    }

    /// One page of the fleet board.
    pub fn fleet(&self, query: &FleetQuery) -> Result<FleetPage, StoreError> {
        let since = query.active_since.unwrap_or(i64::MIN);
        let host = query.host.as_deref();
        let mut counts_query = self.conn.prepare_cached(
            "SELECT status, COUNT(*), SUM(cost) FROM sessions
             WHERE last_ts >= ?1 AND (?2 IS NULL OR host = ?2) GROUP BY status ORDER BY MIN(rank)",
        )?;
        let counts: Vec<StatusCount> = counts_query
            .query_map(params![since, host], |row| {
                Ok(StatusCount {
                    status: row.get(0)?,
                    sessions: row.get::<_, i64>(1)? as u64,
                    cost_usd: row.get::<_, Option<f64>>(2)?.unwrap_or(0.0),
                })
            })?
            .filter_map(Result::ok)
            .collect();
        let hosts: i64 = self.conn.query_row(
            "SELECT COUNT(DISTINCT host) FROM sessions WHERE last_ts >= ?1",
            params![since],
            |row| row.get(0),
        )?;
        // The statuses are matched in SQL through a JSON array: no
        // statement is built from the request.
        let statuses = serde_json::to_string(&query.statuses).unwrap_or_else(|_| "[]".to_owned());
        let any = query.statuses.is_empty();
        // Which rows: one rollup's runs, else (rolled up) the hub's own
        // sessions only, else all.
        let batch = query.batch.as_deref();
        let own_only = query.rollup && batch.is_none();
        let total: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sessions
             WHERE last_ts >= ?1 AND (?2 IS NULL OR host = ?2)
               AND (?3 OR status IN (SELECT value FROM json_each(?4)))
               AND (?5 IS NULL OR batch = ?5) AND (NOT ?6 OR batch IS NULL)",
            params![since, host, any, statuses, batch, own_only],
            |row| row.get(0),
        )?;
        let mut page = self.conn.prepare_cached(
            "SELECT snap FROM sessions
             WHERE last_ts >= ?1 AND (?2 IS NULL OR host = ?2)
               AND (?3 OR status IN (SELECT value FROM json_each(?4)))
               AND (?7 IS NULL OR batch = ?7) AND (NOT ?8 OR batch IS NULL)
             ORDER BY rank, last_ts DESC, host, session LIMIT ?5 OFFSET ?6",
        )?;
        let sessions = page
            .query_map(
                params![
                    since,
                    host,
                    any,
                    statuses,
                    query.limit.clamp(1, FLEET_PAGE_MAX) as i64,
                    query.offset as i64,
                    batch,
                    own_only
                ],
                |row| row.get::<_, String>(0),
            )?
            .filter_map(Result::ok)
            .filter_map(|raw| serde_json::from_str(&raw).ok())
            .collect();
        let batches = if own_only {
            self.rollups(since, host)?
        } else {
            Vec::new()
        };
        Ok(FleetPage {
            sessions,
            total: total as u64,
            counts,
            hosts: hosts as u64,
            batches,
        })
    }

    /// The captured runs' rollups active since `since`, the latest first.
    fn rollups(&self, since: i64, host: Option<&str>) -> Result<Vec<BatchRollup>, StoreError> {
        let mut query = self.conn.prepare_cached(
            "SELECT host, batch, status, COUNT(*), SUM(cost), MIN(last_ts), MAX(last_ts),
                    MAX(json_extract(snap, '$.links.budget_usd')),
                    MAX(json_extract(snap, '$.links.batch')),
                    MAX(json_extract(snap, '$.links.opened_by')),
                    MIN(rank)
             FROM sessions
             WHERE batch IS NOT NULL AND last_ts >= ?1 AND (?2 IS NULL OR host = ?2)
             GROUP BY host, batch, status",
        )?;
        let rows = query.query_map(params![since, host], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                StatusCount {
                    status: row.get(2)?,
                    sessions: row.get::<_, i64>(3)? as u64,
                    cost_usd: row.get::<_, Option<f64>>(4)?.unwrap_or(0.0),
                },
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, Option<f64>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, i64>(10)?,
            ))
        })?;
        let mut rollups: Vec<(BatchRollup, Vec<i64>)> = Vec::new();
        for row in rows {
            let (host, key, count, first, last, budget, label, parent, rank) = row?;
            let at = match rollups
                .iter()
                .position(|(r, _)| r.key == key && r.host == host)
            {
                Some(at) => at,
                None => {
                    let under = key.starts_with("under:");
                    rollups.push((
                        BatchRollup {
                            key,
                            host,
                            label: None,
                            parent: None,
                            parent_name: None,
                            sessions: 0,
                            counts: Vec::new(),
                            cost_usd: 0.0,
                            budget_usd: None,
                            over_budget: false,
                            first_ts: first,
                            last_ts: last,
                        },
                        Vec::new(),
                    ));
                    let (rollup, _) = rollups.last_mut().expect("just pushed");
                    if under {
                        rollup.parent = parent;
                    } else {
                        rollup.label = label;
                    }
                    rollups.len() - 1
                }
            };
            let (rollup, ranks) = &mut rollups[at];
            rollup.sessions += count.sessions;
            rollup.cost_usd += count.cost_usd;
            rollup.first_ts = rollup.first_ts.min(first);
            rollup.last_ts = rollup.last_ts.max(last);
            rollup.budget_usd = match (rollup.budget_usd, budget) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            };
            ranks.push(rank);
            rollup.counts.push(count);
        }
        let mut rollups: Vec<BatchRollup> = rollups
            .into_iter()
            .map(|(mut rollup, ranks)| {
                let mut ordered: Vec<(i64, StatusCount)> =
                    ranks.into_iter().zip(rollup.counts).collect();
                ordered.sort_by_key(|(rank, _)| *rank);
                rollup.counts = ordered.into_iter().map(|(_, c)| c).collect();
                rollup.over_budget = rollup.budget_usd.is_some_and(|b| rollup.cost_usd > b);
                rollup
            })
            .collect();
        rollups.sort_by(|a, b| b.last_ts.cmp(&a.last_ts).then_with(|| a.key.cmp(&b.key)));
        rollups.truncate(ROLLUPS_MAX);
        Ok(rollups)
    }

    /// Forget events and sessions with nothing since `before` (unix ms).
    pub fn prune(&mut self, before: i64) -> Result<usize, StoreError> {
        let tx = self.conn.transaction()?;
        let events = tx.execute("DELETE FROM events WHERE ts < ?1", params![before])?;
        tx.execute("DELETE FROM sessions WHERE last_ts < ?1", params![before])?;
        tx.commit()?;
        Ok(events)
    }
}

/// A status's place on the board: what needs a person first.
fn rank(status: &str) -> i64 {
    match status {
        "blocked" => 0,
        "asking" => 1,
        "failed" => 2,
        "working" => 3,
        "done" => 4,
        "idle" => 5,
        _ => 6,
    }
}

fn read_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<Event>> {
    read_event_at(row, 0)
}

/// An event from a row whose event columns start at `at`; `None` for a
/// kind this build does not know (a newer hub's event on a collector).
fn read_event_at(row: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<Option<Event>> {
    let kind: String = row.get(at + 4)?;
    let body: String = row.get(at + 8)?;
    let Some(what) = serde_json::from_str(&body)
        .ok()
        .and_then(|body| What::from_parts(&kind, body))
    else {
        return Ok(None);
    };
    Ok(Some(Event {
        host: row.get(at)?,
        session: row.get(at + 1)?,
        seq: row.get::<_, i64>(at + 2)? as u64,
        ts: row.get(at + 3)?,
        agent: row.get(at + 5)?,
        work: row.get(at + 6)?,
        item: row.get(at + 7)?,
        what,
    }))
}
