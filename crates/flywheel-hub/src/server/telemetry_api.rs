//! Session telemetry over HTTP (see [`crate::telemetry`]).
//!
//! What agent sessions send, authenticated by the session's own report
//! secret in [`crate::agent::SECRET_HEADER`] (no login, no cookie; these
//! two routes sit outside the origin guard, whose business is browsers):
//!
//! * `POST /api/telemetry/hook`: one Claude Code hook's JSON. An empty
//!   200 tells Claude Code the hook passed; the hub never blocks a tool.
//! * `POST /api/telemetry/mcp`: the progress tool (an MCP server).
//!
//! What the UI, the CLI, and other hubs read and write, with the hub's
//! credential (a peer's included):
//!
//! * `GET /api/telemetry/fleet`: the fleet board, one page.
//! * `GET /api/telemetry/sessions/{id}`: one session in full.
//! * `GET /api/telemetry/sessions/{id}/events`: its event log, paged.
//! * `POST /api/telemetry/batch`: another hub's events (a collector's
//!   intake; see [`crate::telemetry::forward`]).

use super::{ApiQuery, AppState, BodyBound, error_response};
use crate::agent::{SECRET_HEADER, SESSION_HEADER};
use crate::telemetry::{FleetQuery, forward, mcp};
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

/// What an agent may send in one request: a hook's payload carries the
/// tool's whole output (it is cut down before it is kept).
pub(super) const AGENT_BODY: BodyBound = BodyBound {
    limit: 8 * 1024 * 1024,
    timeout: Duration::from_secs(10),
};
/// The most one batch may be on the wire, and inflated.
pub(super) const BATCH_WIRE_MAX: usize = 8 * 1024 * 1024;
const BATCH_INFLATED_MAX: usize = 64 * 1024 * 1024;
/// The fleet board looks back this far unless asked (hours).
const DEFAULT_FLEET_HOURS: i64 = 24;
const DEFAULT_FLEET_LIMIT: usize = 200;
const DEFAULT_EVENTS_LIMIT: usize = 200;

/// The session and secret an agent's request names itself with.
fn agent_of(headers: &HeaderMap) -> Option<(String, String)> {
    let value = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    Some((value(SESSION_HEADER)?, value(SECRET_HEADER)?))
}

fn unnamed() -> Response {
    error_response(
        StatusCode::UNAUTHORIZED,
        "an agent session names itself with the x-flywheel-session and x-flywheel-secret \
         headers its hub gave it",
    )
}

pub(super) async fn hook(State(app): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let Some((session, secret)) = agent_of(&headers) else {
        return unnamed();
    };
    let Ok(payload) = serde_json::from_slice::<Value>(&body) else {
        return error_response(StatusCode::BAD_REQUEST, "the hook payload is not JSON");
    };
    let hub = Arc::clone(&app.hub);
    // Off the async workers: a report reads transcripts and saves state.
    let handled =
        tokio::task::spawn_blocking(move || hub.hook_report(&session, &secret, payload)).await;
    match handled {
        // No body: Claude Code reads an empty 2xx as "the hook passed".
        Ok(Ok(())) => StatusCode::OK.into_response(),
        Ok(Err(message)) => error_response(StatusCode::FORBIDDEN, &message),
        Err(join) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("worker task failed: {join}"),
        ),
    }
}

pub(super) async fn progress_tool(
    State(app): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some((session, secret)) = agent_of(&headers) else {
        return unnamed();
    };
    if !app.hub.agent_authenticated(&session, &secret) {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "the secret does not match that session",
        );
    }
    let Ok(message) = serde_json::from_slice::<Value>(&body) else {
        return Json(json!({
            "jsonrpc": "2.0", "id": null,
            "error": { "code": -32700, "message": "parse error" },
        }))
        .into_response();
    };
    let host = AgentHost {
        hub: Arc::clone(&app.hub),
        session,
    };
    // The acting tools take the hub's locks: off the async runtime.
    let answered = tokio::task::spawn_blocking(move || mcp::answer_body(&message, &host)).await;
    match answered {
        Ok(Some(answer)) => Json(answer).into_response(),
        // Notifications only: accepted, nothing to say.
        Ok(None) => StatusCode::ACCEPTED.into_response(),
        Err(_) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "the tool call failed"),
    }
}

/// The hub, as the session calling a tool sees it.
struct AgentHost {
    hub: Arc<crate::hub::Hub>,
    session: String,
}

impl mcp::Host for AgentHost {
    fn list_sessions(&self) -> String {
        self.hub.agent_sessions(&self.session)
    }

    fn message(&self, to: &str, text: &str) -> Result<String, String> {
        self.hub
            .agent_message(&self.session, to, text)
            .map_err(|e| e.to_string())
    }
}

#[derive(Deserialize, Default)]
pub(super) struct FleetParams {
    /// Comma-separated statuses (`blocked,working`); empty: all.
    status: Option<String>,
    /// One hub's sessions only (its hub id; `local`: this hub's).
    host: Option<String>,
    /// Sessions with an event in the last N hours (`0`: any age).
    hours: Option<String>,
    limit: Option<String>,
    offset: Option<String>,
}

fn number<T: std::str::FromStr>(raw: Option<&str>, name: &str) -> Result<Option<T>, String> {
    match raw.map(str::trim).filter(|r| !r.is_empty()) {
        None => Ok(None),
        Some(raw) => raw
            .parse()
            .map(Some)
            .map_err(|_| format!("{name} must be a whole number, not {raw:?}")),
    }
}

/// The hub id a `host` parameter names (`local` and nothing: this hub).
fn host_of(app: &AppState, raw: Option<&str>) -> Option<String> {
    match raw.map(str::trim).filter(|h| !h.is_empty()) {
        None => None,
        Some("local") => Some(app.hub.telemetry().host().to_owned()),
        Some(host) => Some(host.to_owned()),
    }
}

async fn read<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, crate::telemetry::StoreError> + Send + 'static,
) -> Result<T, Response> {
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => Err(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &e.to_string(),
        )),
        Err(join) => Err(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("worker task failed: {join}"),
        )),
    }
}

pub(super) async fn fleet(
    State(app): State<AppState>,
    ApiQuery(params): ApiQuery<FleetParams>,
) -> Response {
    let parsed = (|| {
        let hours: i64 = number(params.hours.as_deref(), "hours")?.unwrap_or(DEFAULT_FLEET_HOURS);
        Ok::<_, String>(FleetQuery {
            statuses: params
                .status
                .as_deref()
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
            host: host_of(&app, params.host.as_deref()),
            active_since: (hours > 0)
                .then(|| crate::telemetry::now_ms() - hours.saturating_mul(3_600_000)),
            limit: number(params.limit.as_deref(), "limit")?.unwrap_or(DEFAULT_FLEET_LIMIT),
            offset: number(params.offset.as_deref(), "offset")?.unwrap_or(0),
        })
    })();
    let query = match parsed {
        Ok(query) => query,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, &message),
    };
    let telemetry = Arc::clone(app.hub.telemetry());
    let own = telemetry.host().to_owned();
    match read(move || telemetry.fleet(&query)).await {
        Ok(page) => Json(json!({
            // This hub's id: the `host` of the sessions it owns.
            "host": own,
            "now": crate::telemetry::now_ms(),
            "sessions": page.sessions,
            "total": page.total,
            "counts": page.counts,
            "hosts": page.hosts,
        }))
        .into_response(),
        Err(refused) => refused,
    }
}

#[derive(Deserialize, Default)]
pub(super) struct SessionParams {
    host: Option<String>,
    /// Events after this `seq`.
    after: Option<String>,
    limit: Option<String>,
    /// `1`: the newest `limit` events instead of the oldest after `after`.
    newest: Option<String>,
}

pub(super) async fn session(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiQuery(params): ApiQuery<SessionParams>,
) -> Response {
    let host = host_of(&app, params.host.as_deref());
    let telemetry = Arc::clone(app.hub.telemetry());
    let found = tokio::task::spawn_blocking(move || telemetry.detail(host.as_deref(), &id)).await;
    match found {
        Ok(Some(detail)) => Json(json!({
            "now": crate::telemetry::now_ms(),
            "session": detail,
        }))
        .into_response(),
        Ok(None) => error_response(
            StatusCode::NOT_FOUND,
            "no telemetry for that session (it has reported nothing yet)",
        ),
        Err(join) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("worker task failed: {join}"),
        ),
    }
}

pub(super) async fn events(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiQuery(params): ApiQuery<SessionParams>,
) -> Response {
    let (after, limit) = match (
        number::<u64>(params.after.as_deref(), "after"),
        number::<usize>(params.limit.as_deref(), "limit"),
    ) {
        (Ok(after), Ok(limit)) => (after.unwrap_or(0), limit.unwrap_or(DEFAULT_EVENTS_LIMIT)),
        (Err(message), _) | (_, Err(message)) => {
            return error_response(StatusCode::BAD_REQUEST, &message);
        }
    };
    let newest = params.newest.as_deref() == Some("1");
    let host = host_of(&app, params.host.as_deref());
    let telemetry = Arc::clone(app.hub.telemetry());
    match read(move || telemetry.events(host.as_deref(), &id, after, limit, newest)).await {
        Ok(events) => Json(json!({ "events": events })).into_response(),
        Err(refused) => refused,
    }
}

pub(super) async fn batch(
    State(app): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let deflated = headers
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case(forward::DEFLATED));
    let telemetry = Arc::clone(app.hub.telemetry());
    let taken = tokio::task::spawn_blocking(move || {
        let events = forward::decode(&body, deflated, BATCH_INFLATED_MAX)?;
        let received = events.len();
        telemetry
            .ingest(events)
            .map(|accepted| (received, accepted))
    })
    .await;
    match taken {
        Ok(Ok((received, accepted))) => {
            Json(json!({ "received": received, "accepted": accepted })).into_response()
        }
        Ok(Err(message)) => error_response(StatusCode::BAD_REQUEST, &message),
        Err(join) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("worker task failed: {join}"),
        ),
    }
}
