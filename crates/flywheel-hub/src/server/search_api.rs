//! `GET /api/search` (full-text search across sessions) and
//! `GET /api/sessions/{id}/transcript` (a window of an agent session's
//! conversation, where a transcript hit opens).

use super::ApiQuery;
use super::query_flag as flag;
use super::{ApiResult, AppState, blocking, error_response};
use crate::agent::SessionKind;
use crate::search::{
    DEFAULT_LIMIT, DEFAULT_PER_SESSION, MAX_LIMIT, MAX_OFFSET, MAX_PAGE_MESSAGES, MAX_PER_SESSION,
    SearchRequest, Source,
};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Deserialize)]
pub(super) struct SearchQuery {
    q: Option<String>,
    regex: Option<String>,
    case: Option<String>,
    ws: Option<String>,
    session: Option<String>,
    kind: Option<String>,
    source: Option<String>,
    limit: Option<String>,
    per_session: Option<String>,
    offset: Option<String>,
}

fn number(name: &str, raw: Option<&str>, default: usize, max: usize) -> Result<usize, String> {
    match raw {
        None => Ok(default),
        Some(raw) => match raw.parse::<usize>() {
            Ok(n) if (1..=max).contains(&n) => Ok(n),
            _ => Err(format!("{name} must be an integer between 1 and {max}")),
        },
    }
}

/// A comma-separated list (empty entries ignored).
fn list<T>(raw: Option<&str>, parse: impl Fn(&str) -> Result<T, String>) -> Result<Vec<T>, String> {
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(parse)
        .collect()
}

fn request_of(query: &SearchQuery) -> Result<SearchRequest, String> {
    let q = query.q.as_deref().unwrap_or_default();
    if q.is_empty() {
        return Err("q (the query) is required".into());
    }
    let mut request = SearchRequest::new(q);
    request.regex = flag("regex", query.regex.as_deref())?;
    request.case_sensitive = flag("case", query.case.as_deref())?;
    request.workspace = query.ws.clone().filter(|w| !w.is_empty());
    request.session = query.session.clone().filter(|s| !s.is_empty());
    request.kinds = list(query.kind.as_deref(), |k| {
        SessionKind::ALL
            .into_iter()
            .find(|kind| kind.as_str() == k)
            .ok_or_else(|| format!("unknown kind {k:?} (shell, claude, codex)"))
    })?;
    let sources = list(query.source.as_deref(), |s| {
        Source::parse(s)
            .ok_or_else(|| format!("unknown source {s:?} (scrollback, transcript, commands)"))
    })?;
    if !sources.is_empty() {
        request.sources = sources;
    }
    request.limit = number("limit", query.limit.as_deref(), DEFAULT_LIMIT, MAX_LIMIT)?;
    request.per_session = number(
        "per_session",
        query.per_session.as_deref(),
        DEFAULT_PER_SESSION,
        MAX_PER_SESSION,
    )?;
    request.offset = match query.offset.as_deref() {
        None => 0,
        Some(raw) => match raw.parse::<usize>() {
            Ok(n) if n <= MAX_OFFSET => n,
            _ => {
                return Err(format!(
                    "offset must be an integer between 0 and {MAX_OFFSET}"
                ));
            }
        },
    };
    Ok(request)
}

/// Search every session's scrollback, transcript, and suggested commands
/// (filters narrow it); see `crate::search` for the bounds.
pub(super) async fn search(
    State(app): State<AppState>,
    ApiQuery(query): ApiQuery<SearchQuery>,
) -> ApiResult<Response> {
    let request = match request_of(&query) {
        Ok(request) => request,
        Err(message) => return Ok(error_response(StatusCode::BAD_REQUEST, &message)),
    };
    let permit = app.hub.search_index().permit().await;
    let hub = Arc::clone(&app.hub);
    let response = blocking(move || {
        let _permit = permit;
        hub.search(&request)
    })
    .await?;
    Ok(Json(response).into_response())
}

#[derive(Deserialize)]
pub(super) struct TranscriptQuery {
    around: Option<String>,
    before: Option<String>,
    after: Option<String>,
}

/// Messages of an agent session's current conversation around one.
pub(super) async fn transcript(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiQuery(query): ApiQuery<TranscriptQuery>,
) -> ApiResult<Response> {
    let around = match query.around.as_deref() {
        None => None,
        Some(raw) => match raw.parse::<u64>() {
            Ok(n) => Some(n),
            Err(_) => {
                return Ok(error_response(
                    StatusCode::BAD_REQUEST,
                    "around must be a message index (a non-negative integer)",
                ));
            }
        },
    };
    let window = |name: &str, raw: Option<&str>, default: usize| {
        number(name, raw, default, MAX_PAGE_MESSAGES)
    };
    let (before, after) = match (
        window("before", query.before.as_deref(), 30),
        window("after", query.after.as_deref(), 30),
    ) {
        (Ok(b), Ok(a)) => (b, a),
        (Err(message), _) | (_, Err(message)) => {
            return Ok(error_response(StatusCode::BAD_REQUEST, &message));
        }
    };
    let permit = app.hub.search_index().permit().await;
    let hub = Arc::clone(&app.hub);
    let page = blocking(move || {
        let _permit = permit;
        hub.transcript_page(&id, around, before, after)
    })
    .await?;
    Ok(Json(page).into_response())
}
