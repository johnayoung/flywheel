//! Where time and money go: from the finished calls counted per day,
//! session, and shape (`tool_use`, see [`super::shape`]) and the sessions'
//! own totals.
//!
//! * `projects`: per project, sessions, tool time, calls, failures, cost.
//! * `time`: tool time by shape (a shell command by program and verb, any
//!   other tool by name), the most first.
//! * `waiting`: time spent in wait loops (`sleep`, `until`, `while`), and
//!   what the waits cost in model steps: waiting on a run is no waste, but
//!   each wait call is one more step that re-reads the session's context,
//!   so many short waits cost what one long one does not.
//! * `failing`: shapes that keep failing, with their last error.
//! * `rewritten`: files written whole again and again (throwaway scripts
//!   an agent writes anew instead of keeping one).
//! * `context`: what re-reading context costs each session: the tokens it
//!   read back from cache against those it wrote out (and their price at
//!   its model's rate, when its model is known), the most first: where a
//!   handoff or a compact would have saved most.

use super::lanes::project_of;
use super::shape;
use super::store::{Store, StoreError};
use rusqlite::params;
use serde::Serialize;

/// Rows each list keeps.
const LIST_MAX: usize = 15;
/// A shape failing this often is listed as failing.
const FAILING_MIN: i64 = 3;
/// A file written whole this often is listed as rewritten.
const REWRITTEN_MIN: i64 = 3;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Insights {
    pub projects: Vec<ProjectUse>,
    pub time: Vec<ShapeUse>,
    pub waiting: Waiting,
    pub failing: Vec<ShapeUse>,
    pub rewritten: Vec<Rewritten>,
    pub context: ContextTax,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProjectUse {
    pub project: String,
    pub sessions: u64,
    pub calls: u64,
    pub tool_ms: u64,
    pub errors: u64,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShapeUse {
    pub tool: String,
    /// Empty: the tool's calls of every kind.
    pub shape: String,
    pub calls: u64,
    pub ms: u64,
    pub errors: u64,
    pub sessions: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Waiting {
    pub ms: u64,
    pub calls: u64,
    /// Of all shell time.
    pub shell_ms: u64,
    /// Context tokens the steps after the waits re-read, estimated as each
    /// wait call times its session's context now; and their price where
    /// the session's model is known.
    pub reread: u64,
    pub reread_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Rewritten {
    pub path: String,
    pub writes: u64,
    pub sessions: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ContextTax {
    /// Tokens read back from cache, and written out, across sessions.
    pub cache_read: u64,
    pub output: u64,
    /// The cache reads of the sessions whose model is priced, in USD,
    /// and those sessions' whole spend.
    pub cache_read_usd: f64,
    pub priced_cost_usd: f64,
    pub sessions: Vec<SessionTax>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionTax {
    pub host: String,
    pub session: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub project: String,
    pub cache_read: u64,
    pub output: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_usd: Option<f64>,
    pub cost_usd: f64,
    /// The context window in use now (tokens), when reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<u64>,
}

/// The shell tools, as `tool_use` names them.
const SHELLS: &str = "('Bash', 'shell', 'local_shell', 'exec_command', 'shell_command')";

impl Store {
    /// Where time and money went since `since` (of `host`, else every
    /// hub's this store holds; of `project`, else all).
    pub fn insights(
        &self,
        since: i64,
        host: Option<&str>,
        project: Option<&str>,
    ) -> Result<Insights, StoreError> {
        let conn = self.connection();
        // Shell calls by shape; any other tool by name.
        let grouped = format!(
            "SELECT tool, CASE WHEN tool IN {SHELLS} THEN shape ELSE '' END AS kind,
                    SUM(calls), SUM(ms), SUM(errors), COUNT(DISTINCT host || '|' || session),
                    MAX(last_ts), last_error
             FROM tool_use
             WHERE last_ts >= ?1 AND (?2 IS NULL OR host = ?2) AND (?3 IS NULL OR project = ?3)
             GROUP BY tool, kind"
        );
        let shape_use = |row: &rusqlite::Row<'_>| -> rusqlite::Result<ShapeUse> {
            Ok(ShapeUse {
                tool: row.get(0)?,
                shape: row.get(1)?,
                calls: row.get::<_, i64>(2)? as u64,
                ms: row.get::<_, i64>(3)? as u64,
                errors: row.get::<_, i64>(4)? as u64,
                sessions: row.get::<_, i64>(5)? as u64,
                last_error: row.get(7)?,
            })
        };
        let mut all: Vec<ShapeUse> = conn
            .prepare_cached(&grouped)?
            .query_map(params![since, host, project], shape_use)?
            .filter_map(Result::ok)
            .collect();
        let mut waiting = Waiting::default();
        for kind in &all {
            if shell(&kind.tool) {
                waiting.shell_ms += kind.ms;
                if shape::is_poll(&kind.shape) {
                    waiting.ms += kind.ms;
                    waiting.calls += kind.calls;
                }
            }
        }
        let rereads = conn
            .prepare_cached(&format!(
                "SELECT SUM(t.calls), json_extract(s.snap, '$.context.used'),
                        json_extract(s.snap, '$.model')
                 FROM tool_use t JOIN sessions s ON s.host = t.host AND s.session = t.session
                 WHERE t.tool IN {SHELLS} AND (t.shape = 'poll' OR t.shape LIKE 'poll %')
                   AND t.last_ts >= ?1 AND (?2 IS NULL OR t.host = ?2)
                   AND (?3 IS NULL OR t.project = ?3)
                 GROUP BY t.host, t.session"
            ))?
            .query_map(params![since, host, project], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        for (calls, used, model) in rereads {
            let read = calls.max(0) as u64 * used.map_or(0, |u| u.max(0) as u64);
            waiting.reread += read;
            if let Some(rate) = cache_read_rate(model.as_deref()) {
                waiting.reread_usd += read as f64 * rate / 1_000_000.0;
            }
        }
        let mut failing: Vec<ShapeUse> = all
            .iter()
            .filter(|k| k.errors >= FAILING_MIN as u64)
            .cloned()
            .collect();
        failing.sort_by(|a, b| b.errors.cmp(&a.errors).then(b.calls.cmp(&a.calls)));
        failing.truncate(LIST_MAX);
        all.sort_by(|a, b| b.ms.cmp(&a.ms));
        all.truncate(LIST_MAX);
        for kind in all.iter_mut().chain(failing.iter_mut()) {
            if kind.errors == 0 {
                kind.last_error = None;
            }
            if shell(&kind.tool) && shape::is_poll(&kind.shape) {
                kind.shape = kind.shape.replacen(shape::POLL, WAIT_LABEL, 1);
            }
        }

        let rewritten: Vec<Rewritten> = conn
            .prepare_cached(
                "SELECT shape, SUM(calls), COUNT(DISTINCT host || '|' || session) FROM tool_use
                 WHERE tool = 'Write' AND shape != '' AND last_ts >= ?1
                   AND (?2 IS NULL OR host = ?2) AND (?3 IS NULL OR project = ?3)
                 GROUP BY shape HAVING SUM(calls) >= ?4
                 ORDER BY SUM(calls) DESC, shape LIMIT ?5",
            )?
            .query_map(
                params![since, host, project, REWRITTEN_MIN, LIST_MAX as i64],
                |row| {
                    Ok(Rewritten {
                        path: row.get(0)?,
                        writes: row.get::<_, i64>(1)? as u64,
                        sessions: row.get::<_, i64>(2)? as u64,
                    })
                },
            )?
            .filter_map(Result::ok)
            .collect();

        let mut projects: Vec<ProjectUse> = conn
            .prepare_cached(
                "SELECT project, COUNT(DISTINCT host || '|' || session), SUM(calls), SUM(ms),
                        SUM(errors)
                 FROM tool_use
                 WHERE last_ts >= ?1 AND (?2 IS NULL OR host = ?2) AND (?3 IS NULL OR project = ?3)
                 GROUP BY project",
            )?
            .query_map(params![since, host, project], |row| {
                Ok(ProjectUse {
                    project: row.get(0)?,
                    sessions: row.get::<_, i64>(1)? as u64,
                    calls: row.get::<_, i64>(2)? as u64,
                    tool_ms: row.get::<_, i64>(3)? as u64,
                    errors: row.get::<_, i64>(4)? as u64,
                    cost_usd: 0.0,
                })
            })?
            .filter_map(Result::ok)
            .collect();

        // Cost and context, from each session's totals.
        let mut context = ContextTax::default();
        let mut sessions = conn.prepare_cached(
            "SELECT host, session, cost, json_extract(snap, '$.dir'), json_extract(snap, '$.label'),
                    json_extract(snap, '$.model'), json_extract(snap, '$.usage.cache_read'),
                    json_extract(snap, '$.context.used'), json_extract(snap, '$.usage.output')
             FROM sessions WHERE last_ts >= ?1 AND (?2 IS NULL OR host = ?2)",
        )?;
        let rows = sessions.query_map(params![since, host], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, f64>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, Option<i64>>(7)?,
                row.get::<_, Option<i64>>(8)?,
            ))
        })?;
        for row in rows.filter_map(Result::ok) {
            let (host, session, cost, dir, label, model, cache_read, used, output) = row;
            let place = project_of(dir.as_deref());
            if project.is_some_and(|p| p != place) {
                continue;
            }
            match projects.iter_mut().find(|p| p.project == place) {
                Some(found) => found.cost_usd += cost,
                None => projects.push(ProjectUse {
                    project: place.clone(),
                    sessions: 1,
                    calls: 0,
                    tool_ms: 0,
                    errors: 0,
                    cost_usd: cost,
                }),
            }
            let Some(read) = cache_read.map(|r| r.max(0) as u64).filter(|r| *r > 0) else {
                continue;
            };
            let output = output.map_or(0, |o| o.max(0) as u64);
            // `claude-opus-5-5[1m]`: the model is what is priced.
            let rate = cache_read_rate(model.as_deref());
            let read_usd = rate.map(|rate| read as f64 * rate / 1_000_000.0);
            context.cache_read += read;
            context.output += output;
            if let Some(usd) = read_usd {
                context.cache_read_usd += usd;
                context.priced_cost_usd += cost;
            }
            context.sessions.push(SessionTax {
                host,
                session,
                label,
                project: place,
                cache_read: read,
                output,
                cache_read_usd: read_usd,
                cost_usd: cost,
                context: used.map(|u| u.max(0) as u64),
            });
        }
        context
            .sessions
            .sort_by(|a, b| b.cache_read.cmp(&a.cache_read));
        context.sessions.truncate(LIST_MAX);
        projects.sort_by(|a, b| {
            b.cost_usd
                .total_cmp(&a.cost_usd)
                .then(b.tool_ms.cmp(&a.tool_ms))
        });
        Ok(Insights {
            projects,
            time: all,
            waiting,
            failing,
            rewritten,
            context,
        })
    }
}

/// What a wait loop is called in the lists.
const WAIT_LABEL: &str = "wait loop";

/// A model's cache-read price per million tokens (`claude-opus-5-5[1m]`:
/// the model is what is priced).
fn cache_read_rate(model: Option<&str>) -> Option<f64> {
    model
        .map(|m| m.split('[').next().unwrap_or(m))
        .and_then(crate::usage::price_of)
        .map(|price| price.base.cache_read)
}

fn shell(tool: &str) -> bool {
    matches!(
        tool,
        "Bash" | "shell" | "local_shell" | "exec_command" | "shell_command"
    )
}
