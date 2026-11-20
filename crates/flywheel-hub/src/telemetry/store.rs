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
const SCHEMA: i64 = 5;
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
    /// Only sessions whose labels hold every term (`fw.state` terms
    /// match the status).
    pub labels: Vec<crate::labels::Term>,
}

/// SQL true when the `sessions` row's labels hold every term of the JSON
/// array parameter `?N` (`[{"key","values"?}]`): no statement is built
/// from a request.
fn labels_clause(n: usize) -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM json_each(?{n}) t WHERE NOT EXISTS (
             SELECT 1 FROM session_labels l
             WHERE l.host = sessions.host AND l.session = sessions.session
               AND l.key = json_extract(t.value, '$.key')
               AND (COALESCE(json_array_length(t.value, '$.values'), 0) = 0
                    OR l.value IN (SELECT value FROM json_each(t.value, '$.values')))))"
    )
}

/// A query's statuses with its `fw.state` terms folded in, and its other
/// label terms as the JSON array [`labels_clause`] reads.
fn status_and_labels(statuses: &[String], terms: &[crate::labels::Term]) -> (Vec<String>, String) {
    let mut statuses = statuses.to_vec();
    for term in terms.iter().filter(|t| t.key == crate::labels::STATE) {
        statuses = if statuses.is_empty() {
            term.values.clone()
        } else {
            statuses
                .into_iter()
                .filter(|s| term.values.contains(s))
                .collect()
        };
        if statuses.is_empty() {
            // Terms no status satisfies: none matches.
            statuses.push(String::new());
        }
    }
    let rest: Vec<&crate::labels::Term> = terms
        .iter()
        .filter(|t| t.key != crate::labels::STATE)
        .collect();
    let json = serde_json::to_string(&rest).unwrap_or_else(|_| "[]".to_owned());
    (statuses, json)
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
    /// The connection, for the queries kept beside the store.
    pub(super) fn connection(&self) -> &Connection {
        &self.conn
    }

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
        // Schema 3: finished tool calls counted per day, session, and
        // shape (see `super::insights`), from the events already kept.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS tool_use (
                 day        INTEGER NOT NULL,
                 host       TEXT NOT NULL,
                 session    TEXT NOT NULL,
                 tool       TEXT NOT NULL,
                 shape      TEXT NOT NULL,
                 project    TEXT NOT NULL,
                 calls      INTEGER NOT NULL,
                 ms         INTEGER NOT NULL,
                 errors     INTEGER NOT NULL,
                 last_error TEXT,
                 last_ts    INTEGER NOT NULL,
                 PRIMARY KEY (day, host, session, tool, shape)
             ) WITHOUT ROWID;
             CREATE INDEX IF NOT EXISTS tool_use_ts ON tool_use (last_ts);",
        )?;
        // Schema 4: each session's labels, for filters in SQL (the fleet,
        // the event log), kept from its `labels.changed` events.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS session_labels (
                 host    TEXT NOT NULL,
                 session TEXT NOT NULL,
                 key     TEXT NOT NULL,
                 value   TEXT NOT NULL,
                 PRIMARY KEY (host, session, key)
             ) WITHOUT ROWID;
             CREATE INDEX IF NOT EXISTS session_labels_value ON session_labels (key, value);",
        )?;
        // Schema 5: the event log (see `super::log`): the status an event
        // moved its session to, and indexes for the log's filters.
        let has_state: bool = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('events') WHERE name = 'state'",
            [],
            |row| row.get::<_, i64>(0).map(|n| n > 0),
        )?;
        if !has_state {
            conn.execute_batch("ALTER TABLE events ADD COLUMN state TEXT")?;
        }
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS events_session_id ON events (session, id);
             CREATE INDEX IF NOT EXISTS events_state ON events (state, id) WHERE state IS NOT NULL;
             CREATE INDEX IF NOT EXISTS events_kind ON events (kind, id);",
        )?;
        // The log's epoch: cursors from another store (a wiped one) are
        // told apart, never read as this one's rows.
        conn.execute(
            "INSERT OR IGNORE INTO cursors (name, at) VALUES ('log.epoch', ABS(RANDOM() % 4294967295) + 1)",
            [],
        )?;
        let mut store = Self { conn };
        if found < 3 {
            store.backfill_tool_use()?;
        }
        store.conn.pragma_update(None, "user_version", SCHEMA)?;
        Ok(store)
    }

    /// Append `events` and save `states`, in one transaction.
    pub fn write(&mut self, events: &[Event], states: &[SessionState]) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        {
            let mut insert = tx.prepare_cached(
                "INSERT OR IGNORE INTO events (host, session, seq, ts, kind, agent, work, item, body, state)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?;
            let projects: std::collections::HashMap<(&str, &str), String> = states
                .iter()
                .map(|state| {
                    let snap = state.snapshot();
                    (
                        (state.host.as_str(), state.session.as_str()),
                        super::lanes::project_of(snap.dir.as_deref()),
                    )
                })
                .collect();
            let mut counted = tx.prepare_cached(TOOL_USE_UPSERT)?;
            let mut label_set = tx.prepare_cached(
                "INSERT INTO session_labels (host, session, key, value) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (host, session, key) DO UPDATE SET value = excluded.value",
            )?;
            let mut label_unset = tx.prepare_cached(
                "DELETE FROM session_labels WHERE host = ?1 AND session = ?2 AND key = ?3",
            )?;
            for event in events {
                let new = insert.execute(params![
                    event.host,
                    event.session,
                    event.seq as i64,
                    event.ts,
                    event.what.stored_kind(),
                    event.agent,
                    event.work,
                    event.item,
                    event.what.stored_body().to_string(),
                    event.moved,
                ])?;
                if new == 1
                    && let What::LabelsChanged { set, .. } = &event.what
                {
                    for (key, value) in set {
                        match value {
                            Some(value) => {
                                label_set.execute(params![event.host, event.session, key, value])?
                            }
                            None => label_unset.execute(params![event.host, event.session, key])?,
                        };
                    }
                }
                // A finished call is counted once, as its event is kept.
                if new == 1
                    && let Some(call) = Call::of(event)
                {
                    let project = projects
                        .get(&(event.host.as_str(), event.session.as_str()))
                        .cloned()
                        .unwrap_or_else(|| super::lanes::project_of(None));
                    call.count(&mut counted, event, &project)?;
                }
            }
            let mut upsert = tx.prepare_cached(
                "INSERT INTO sessions (host, session, status, rank, last_ts, cost, snap, state, batch)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT (host, session) DO UPDATE SET
                     status = excluded.status, rank = excluded.rank, last_ts = excluded.last_ts,
                     cost = excluded.cost, snap = excluded.snap, state = excluded.state,
                     batch = excluded.batch",
            )?;
            // A session only the hub recorded (a labelled shell) is not
            // on the board.
            for state in states.iter().filter(|s| s.observed()) {
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

    /// The log's last row (0: none).
    pub fn head(&self) -> Result<i64, StoreError> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(MAX(id), 0) FROM events", [], |row| {
                row.get(0)
            })?)
    }

    /// The highest `seq` kept for a session (0: none): where a session
    /// whose state was never saved goes on numbering.
    pub fn max_seq(&self, host: &str, session: &str) -> Result<u64, StoreError> {
        let seq: Option<i64> = self.conn.query_row(
            "SELECT MAX(seq) FROM events WHERE host = ?1 AND session = ?2",
            params![host, session],
            |row| row.get(0),
        )?;
        Ok(seq.unwrap_or(0).max(0) as u64)
    }

    /// A session's labels as kept.
    pub fn labels(&self, host: &str, session: &str) -> Result<crate::labels::Labels, StoreError> {
        let mut query = self.conn.prepare_cached(
            "SELECT key, value FROM session_labels WHERE host = ?1 AND session = ?2",
        )?;
        let rows = query.query_map(params![host, session], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        Ok(rows.filter_map(Result::ok).collect())
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
             WHERE host = ?1 AND session = ?2 AND seq > ?3
               AND kind IN (SELECT value FROM json_each(?5))
             ORDER BY seq {order} LIMIT ?4"
        ))?;
        let mut events: Vec<Event> = query
            .query_map(
                params![host, session, after as i64, limit, own_kinds()],
                read_event,
            )?
            .filter_map(|row| row.ok().flatten())
            .collect();
        if newest {
            events.reverse();
        }
        Ok(events)
    }

    /// The newest `limit` leases released (`lease.released` events), as
    /// (resource, how long it was held in ms).
    pub fn lease_holds(&self, limit: usize) -> Result<Vec<(String, i64)>, StoreError> {
        let mut query = self.conn.prepare_cached(
            "SELECT json_extract(body, '$.resource'), ts - json_extract(body, '$.granted_at')
             FROM events WHERE kind = 'lease.released'
             ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = query.query_map(params![limit as i64], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<i64>>(1)?,
            ))
        })?;
        Ok(rows
            .filter_map(Result::ok)
            .filter_map(|(resource, held)| Some((resource?, held.filter(|h| *h >= 0)?)))
            .collect())
    }

    /// A session's events at or after `from` (unix ms), oldest first: the
    /// last `cap` of them.
    pub fn events_from(
        &self,
        host: &str,
        session: &str,
        from: i64,
        cap: usize,
    ) -> Result<Vec<Event>, StoreError> {
        let mut query = self.conn.prepare_cached(
            "SELECT host, session, seq, ts, kind, agent, work, item, body FROM events
             WHERE host = ?1 AND session = ?2 AND ts >= ?3
               AND kind IN (SELECT value FROM json_each(?5))
             ORDER BY seq DESC LIMIT ?4",
        )?;
        let mut events: Vec<Event> = query
            .query_map(
                params![host, session, from, cap as i64, own_kinds()],
                read_event,
            )?
            .filter_map(|row| row.ok().flatten())
            .collect();
        events.reverse();
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
        let (statuses, labels) = status_and_labels(&query.statuses, &query.labels);
        let mut counts_query = self.conn.prepare_cached(&format!(
            "SELECT status, COUNT(*), SUM(cost) FROM sessions
             WHERE last_ts >= ?1 AND (?2 IS NULL OR host = ?2) AND {}
             GROUP BY status ORDER BY MIN(rank)",
            labels_clause(3)
        ))?;
        let counts: Vec<StatusCount> = counts_query
            .query_map(params![since, host, labels], |row| {
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
        let any = statuses.is_empty();
        let statuses = serde_json::to_string(&statuses).unwrap_or_else(|_| "[]".to_owned());
        // Which rows: one rollup's runs, else (rolled up) the hub's own
        // sessions only, else all.
        let batch = query.batch.as_deref();
        let own_only = query.rollup && batch.is_none();
        let total: i64 = self.conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM sessions
                 WHERE last_ts >= ?1 AND (?2 IS NULL OR host = ?2)
                   AND (?3 OR status IN (SELECT value FROM json_each(?4)))
                   AND (?5 IS NULL OR batch = ?5) AND (NOT ?6 OR batch IS NULL) AND {}",
                labels_clause(7)
            ),
            params![since, host, any, statuses, batch, own_only, labels],
            |row| row.get(0),
        )?;
        let mut page = self.conn.prepare_cached(&format!(
            "SELECT snap FROM sessions
             WHERE last_ts >= ?1 AND (?2 IS NULL OR host = ?2)
               AND (?3 OR status IN (SELECT value FROM json_each(?4)))
               AND (?7 IS NULL OR batch = ?7) AND (NOT ?8 OR batch IS NULL) AND {}
             ORDER BY rank, last_ts DESC, host, session LIMIT ?5 OFFSET ?6",
            labels_clause(9)
        ))?;
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
                    own_only,
                    labels
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
        // A session labelled `eval` or `fw.eval` (an eval run's trial) is
        // kept past the window: its trace is the eval's evidence.
        let pinned = |table: &str| {
            format!(
                "NOT EXISTS (SELECT 1 FROM session_labels p WHERE p.key IN ('{key}', '{run}')
                     AND p.host = {table}.host AND p.session = {table}.session)",
                key = crate::labels::EVAL,
                run = crate::labels::EVAL_RUN,
            )
        };
        let events = tx.execute(
            &format!("DELETE FROM events WHERE ts < ?1 AND {}", pinned("events")),
            params![before],
        )?;
        tx.execute(
            &format!(
                "DELETE FROM sessions WHERE last_ts < ?1 AND {}",
                pinned("sessions")
            ),
            params![before],
        )?;
        tx.execute(
            &format!(
                "DELETE FROM tool_use WHERE last_ts < ?1 AND {}",
                pinned("tool_use")
            ),
            params![before],
        )?;
        tx.execute(
            "DELETE FROM session_labels WHERE NOT EXISTS (SELECT 1 FROM events e
                 WHERE e.host = session_labels.host AND e.session = session_labels.session)",
            [],
        )?;
        tx.commit()?;
        Ok(events)
    }
}

const TOOL_USE_UPSERT: &str = "INSERT INTO tool_use
         (day, host, session, tool, shape, project, calls, ms, errors, last_error, last_ts)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8, ?9, ?10)
     ON CONFLICT (day, host, session, tool, shape) DO UPDATE SET
         calls = calls + 1, ms = ms + excluded.ms, errors = errors + excluded.errors,
         last_error = COALESCE(excluded.last_error, last_error),
         last_ts = MAX(last_ts, excluded.last_ts), project = excluded.project";

/// One finished tool call, as counted.
struct Call<'a> {
    tool: &'a str,
    shape: String,
    ms: u64,
    error: Option<&'a str>,
    failed: bool,
}

impl<'a> Call<'a> {
    /// The call a `tool.finished` event closes (a denied one never ran).
    fn of(event: &'a Event) -> Option<Self> {
        let What::ToolFinished {
            name,
            summary,
            outcome,
            duration_ms,
            error,
            ..
        } = &event.what
        else {
            return None;
        };
        if *outcome == super::event::Outcome::Denied {
            return None;
        }
        Some(Self {
            tool: name,
            shape: super::shape::of(name, summary.as_deref()),
            ms: duration_ms.unwrap_or(0),
            error: error.as_deref(),
            failed: *outcome == super::event::Outcome::Error,
        })
    }

    fn count(
        &self,
        upsert: &mut rusqlite::CachedStatement<'_>,
        event: &Event,
        project: &str,
    ) -> rusqlite::Result<usize> {
        upsert.execute(params![
            event.ts.div_euclid(DAY_MS),
            event.host,
            event.session,
            self.tool,
            self.shape,
            project,
            i64::try_from(self.ms).unwrap_or(i64::MAX),
            i64::from(self.failed),
            self.error.filter(|_| self.failed),
            event.ts,
        ])
    }
}

/// A day in milliseconds (`tool_use` counts by UTC day).
pub const DAY_MS: i64 = 24 * 60 * 60 * 1000;

impl Store {
    /// Count the finished calls of the events already kept (a store from
    /// before `tool_use`): each finish paired with its start, as the
    /// reducer pairs them, session by session.
    fn backfill_tool_use(&mut self) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        {
            let mut dirs =
                tx.prepare("SELECT host, session, json_extract(snap, '$.dir') FROM sessions")?;
            let projects: std::collections::HashMap<(String, String), String> = dirs
                .query_map([], |row| {
                    Ok((
                        (row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                        super::lanes::project_of(row.get::<_, Option<String>>(2)?.as_deref()),
                    ))
                })?
                .filter_map(Result::ok)
                .collect();
            let mut scan = tx.prepare(
                "SELECT host, session, seq, ts, kind, agent, work, item, body FROM events
                 WHERE kind IN ('tool.started', 'tool.finished') ORDER BY host, session, seq",
            )?;
            let mut upsert = tx.prepare_cached(TOOL_USE_UPSERT)?;
            let mut open: std::collections::HashMap<String, (String, Option<String>, i64)> =
                std::collections::HashMap::new();
            let mut current: Option<(String, String)> = None;
            let mut rows = scan.query([])?;
            while let Some(row) = rows.next()? {
                let Some(mut event) = read_event(row)? else {
                    continue;
                };
                let key = (event.host.clone(), event.session.clone());
                if current.as_ref() != Some(&key) {
                    open.clear();
                    current = Some(key.clone());
                }
                match &mut event.what {
                    What::ToolStarted {
                        id, name, summary, ..
                    } => {
                        open.insert(id.clone(), (name.clone(), summary.clone(), event.ts));
                    }
                    What::ToolFinished {
                        id,
                        name,
                        summary,
                        duration_ms,
                        ..
                    } => {
                        let started = open.remove(id.as_str()).or_else(|| {
                            // A finish without a known id: the oldest call of
                            // its name.
                            let by_name = open
                                .iter()
                                .filter(|(_, (n, _, _))| name.is_empty() || n == name)
                                .min_by_key(|(_, (_, _, ts))| *ts)
                                .map(|(id, _)| id.clone())?;
                            open.remove(&by_name)
                        });
                        if let Some((started_name, started_summary, at)) = started {
                            if name.is_empty() {
                                *name = started_name;
                            }
                            if summary.is_none() {
                                *summary = started_summary;
                            }
                            if duration_ms.is_none() {
                                *duration_ms = u64::try_from(event.ts - at).ok();
                            }
                        }
                    }
                    _ => {}
                }
                if let Some(call) = Call::of(&event) {
                    let project = projects
                        .get(&key)
                        .cloned()
                        .unwrap_or_else(|| super::lanes::project_of(None));
                    call.count(&mut upsert, &event, &project)?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }
}

/// A status's place on the board: what needs a person first.
fn rank(status: &str) -> i64 {
    match status {
        "blocked" => 0,
        "asking" => 1,
        "failed" => 2,
        "compacting" | "working" => 3,
        "background" => 4,
        "done" => 5,
        "idle" => 6,
        _ => 7,
    }
}

/// The kinds an agent's own timeline reads (every telemetry kind; hub
/// events are the log's), as the JSON array the queries take.
fn own_kinds() -> &'static str {
    static KINDS: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    KINDS.get_or_init(|| serde_json::to_string(super::event::KINDS).unwrap_or_default())
}

pub(super) fn read_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<Event>> {
    read_event_at(row, 0)
}

/// An event from a row whose event columns start at `at`; `None` for a
/// kind this build does not know (a newer hub's event on a collector).
pub(super) fn read_event_at(row: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<Option<Event>> {
    let kind: String = row.get(at + 4)?;
    let body: String = row.get(at + 8)?;
    let Some(what) = serde_json::from_str(&body)
        .ok()
        .and_then(|body| What::from_stored(&kind, body))
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
        moved: None,
    }))
}
