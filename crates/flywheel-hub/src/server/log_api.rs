//! The event log over HTTP (see [`crate::telemetry::log`]): `GET
//! /api/log`, one read by cursor and filter, and with `wait=SECS` a long
//! poll that answers as soon as a matching event is in (or the time is
//! up, with none and the cursor to go on from).

use super::{ApiQuery, ApiResult, AppState, blocking};
use crate::eventlog::Filter;
use crate::hub::HubError;
use crate::state::StateError;
use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;

/// The longest a read waits for a matching event.
pub const MAX_WAIT: Duration = Duration::from_secs(60);
/// Rows a read returns when it names no limit.
const DEFAULT_LIMIT: usize = 100;

#[derive(Deserialize, Default)]
pub(super) struct LogParams {
    after: Option<String>,
    /// A whole filter (`state=done children=lead team=web`).
    filter: Option<String>,
    /// Comma lists, each one field of the filter.
    session: Option<String>,
    ws: Option<String>,
    state: Option<String>,
    kind: Option<String>,
    label: Option<String>,
    /// One hub's rows (a collector's log holds every hub's).
    host: Option<String>,
    limit: Option<String>,
    /// Seconds to wait for a match (at most 60).
    wait: Option<String>,
}

fn invalid(message: String) -> HubError {
    StateError::Invalid(message).into()
}

/// The params as one filter.
fn filter_of(params: &LogParams) -> Result<Filter, HubError> {
    let mut filter = Filter::parse(params.filter.as_deref().unwrap_or("")).map_err(invalid)?;
    let list = |raw: &Option<String>| -> Vec<String> {
        raw.as_deref()
            .unwrap_or("")
            .split([',', '|'])
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .collect()
    };
    filter.sessions.extend(list(&params.session));
    filter.workspaces.extend(list(&params.ws));
    filter.kinds.extend(list(&params.kind));
    for state in list(&params.state) {
        let more = Filter::parse(&format!("state={state}")).map_err(invalid)?;
        filter.states.extend(more.states);
    }
    filter.labels.extend(
        crate::labels::parse_terms(&[params.label.clone().unwrap_or_default()])
            .map_err(|e| invalid(e.to_string()))?,
    );
    Ok(filter)
}

fn number(raw: Option<&str>, name: &str) -> Result<Option<u64>, HubError> {
    match raw.map(str::trim).filter(|r| !r.is_empty()) {
        None => Ok(None),
        Some(raw) => raw
            .parse()
            .map(Some)
            .map_err(|_| invalid(format!("{name} must be a whole number, not {raw:?}"))),
    }
}

pub(super) async fn read_log(
    State(app): State<AppState>,
    ApiQuery(params): ApiQuery<LogParams>,
) -> ApiResult<Response> {
    let filter = filter_of(&params)?;
    let limit = number(params.limit.as_deref(), "limit")?.map_or(DEFAULT_LIMIT, |n| n as usize);
    let wait = number(params.wait.as_deref(), "wait")?
        .map_or(Duration::ZERO, Duration::from_secs)
        .min(MAX_WAIT);
    let hub = Arc::clone(&app.hub);
    let after = params.after.clone();
    let host = params.host.clone().filter(|h| !h.trim().is_empty());
    let mut query = blocking(move || {
        let filter = hub.resolve_filter(&filter, None)?;
        let after = hub.log_cursor(after.as_deref())?;
        let mut query = hub.log_query(&filter, after, limit);
        query.host = match host.as_deref() {
            Some("local") => Some(hub.telemetry().host().to_owned()),
            other => other.map(str::to_owned),
        };
        Ok(query)
    })
    .await?;
    let deadline = tokio::time::Instant::now() + wait;
    let mut moves = app.hub.telemetry().log_moves();
    loop {
        // Seen before the read: a commit during it still wakes the wait.
        moves.borrow_and_update();
        let hub = Arc::clone(&app.hub);
        let asked = query.clone();
        let page = blocking(move || hub.read_log(&asked)).await?;
        // Waiting: only for a read from a cursor that found nothing yet.
        let waiting = !wait.is_zero() && query.after.is_some() && page.events.is_empty();
        if !waiting || page.more || tokio::time::Instant::now() >= deadline {
            return Ok(Json(page).into_response());
        }
        let head = app.hub.telemetry().log_epoch().map_err(HubError::from)?;
        query.after = Some(
            crate::telemetry::log::Cursor::parse(&page.cursor, head)
                .map_err(invalid)?
                .at,
        );
        if tokio::time::timeout_at(deadline, moves.changed())
            .await
            .is_err()
        {
            let hub = Arc::clone(&app.hub);
            let asked = query.clone();
            let page = blocking(move || hub.read_log(&asked)).await?;
            return Ok(Json(page).into_response());
        }
    }
}
