//! The event log on the hub (see [`crate::telemetry::log`]): filters
//! resolved against this hub's sessions and workspaces, cursors checked
//! against its store, rows named.

use super::{Hub, HubError};
use crate::eventlog::Filter;
use crate::labels::Term;
use crate::state::StateError;
use crate::telemetry::log::{Cursor, LogPage, LogQuery};

/// A session id no session has: a filter that resolved to no session
/// selects nothing (an empty list would select everything).
const NONE: &str = "-";

impl Hub {
    /// `raw` as a row id of this hub's log: `now` (or `end`) is its end;
    /// `None` stays `None` (the newest rows).
    pub fn log_cursor(&self, raw: Option<&str>) -> Result<Option<i64>, HubError> {
        let Some(raw) = raw.map(str::trim).filter(|r| !r.is_empty()) else {
            return Ok(None);
        };
        let head = self.telemetry().log_head()?;
        match raw {
            "now" | "end" => Ok(Some(head.at)),
            "start" | "0" => Ok(Some(0)),
            raw => Cursor::parse(raw, head.epoch)
                .map(|c| Some(c.at))
                .map_err(|e| StateError::Invalid(e).into()),
        }
    }

    /// `filter` with its sessions, workspaces, and children named by id
    /// (`owner`: whose bare `children` they are).
    pub fn resolve_filter(&self, filter: &Filter, owner: Option<&str>) -> Result<Filter, HubError> {
        let store = self.store();
        let mut resolved = filter.clone();
        let session_id = |query: &str| -> Result<String, HubError> {
            crate::script::find_by(
                store.sessions(),
                |s| (s.id.as_str(), s.name.as_str()),
                query,
                "session",
            )
            .map_err(StateError::Invalid)?
            .map(|s| s.id.clone())
            .ok_or_else(|| StateError::Invalid(format!("no session matches {query:?}")).into())
        };
        resolved.sessions = filter
            .sessions
            .iter()
            .map(|s| session_id(s))
            .collect::<Result<_, _>>()?;
        resolved.workspaces = filter
            .workspaces
            .iter()
            .map(|w| {
                crate::script::find_by(
                    store.workspaces(),
                    |ws| (ws.id.as_str(), ws.name.as_str()),
                    w,
                    "workspace",
                )
                .map_err(StateError::Invalid)?
                .map(|ws| ws.id.clone())
                .ok_or_else(|| StateError::Invalid(format!("no workspace matches {w:?}")).into())
            })
            .collect::<Result<_, HubError>>()?;
        resolved.children = match filter.children.as_deref() {
            None => None,
            Some("") => Some(
                owner
                    .ok_or_else(|| {
                        StateError::Invalid("children of whom? children=SESSION".into())
                    })?
                    .to_owned(),
            ),
            Some(of) => Some(session_id(of)?),
        };
        Ok(resolved)
    }

    /// A resolved filter as a read of the log (workspaces become their
    /// sessions now, children a `fw.parent` term).
    pub fn log_query(&self, filter: &Filter, after: Option<i64>, limit: usize) -> LogQuery {
        let mut sessions = filter.sessions.clone();
        if !filter.workspaces.is_empty() {
            let store = self.store();
            let of_ws: Vec<String> = store
                .sessions()
                .iter()
                .filter(|s| filter.workspaces.contains(&s.workspace_id))
                .map(|s| s.id.clone())
                .collect();
            sessions = if sessions.is_empty() {
                of_ws
            } else {
                sessions.into_iter().filter(|s| of_ws.contains(s)).collect()
            };
            if sessions.is_empty() {
                sessions.push(NONE.to_owned());
            }
        }
        let mut labels = filter.labels.clone();
        if let Some(parent) = &filter.children {
            labels.push(Term {
                key: crate::labels::PARENT.to_owned(),
                values: vec![parent.clone()],
            });
        }
        LogQuery {
            after,
            sessions,
            states: filter.states.clone(),
            kinds: filter.kinds.clone(),
            labels,
            host: None,
            not_sessions: Vec::new(),
            limit,
        }
    }

    /// One read of the log, each row of this hub's own sessions named.
    pub fn read_log(&self, query: &LogQuery) -> Result<LogPage, HubError> {
        let mut page = self.telemetry().log(query)?;
        let own = self.telemetry().host().to_owned();
        let store = self.store();
        for row in page.events.iter_mut().filter(|r| r.host == own) {
            if let Ok(record) = store.session(&row.session) {
                row.name = Some(record.name.clone());
                row.workspace = store
                    .workspace(&record.workspace_id)
                    .ok()
                    .map(|w| w.name.clone());
            }
        }
        Ok(page)
    }
}
