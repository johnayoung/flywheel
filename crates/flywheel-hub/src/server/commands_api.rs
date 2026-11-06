//! `GET /api/sessions/{id}/commands`, `GET /api/sessions/{id}/replies`,
//! `POST /api/sessions/{id}/paste`, and the events socket's `commands` and
//! `replies` messages.

use super::ApiQuery;
use super::{ApiJson, ApiResult, AppState, blocking, error_response, text};
use crate::commands::Command;
use crate::replies::{AgentReply, ReplyView};
use crate::transcript::{CommandFilter, MAX_COMMANDS, MAX_REPLIES, is_fresh};
use axum::Json;
use axum::extract::ws::Message;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

/// Default `limit`.
pub const DEFAULT_LIMIT: usize = 50;

#[derive(Deserialize)]
pub(super) struct CommandsQuery {
    /// Parsed by hand so a bad value gets the API's JSON error.
    limit: Option<String>,
    all: Option<String>,
}

/// A session's suggested commands, newest first: shell blocks only unless
/// `all=true`; at most `limit` (1..=500, default 50).
pub(super) async fn list_commands(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiQuery(query): ApiQuery<CommandsQuery>,
) -> ApiResult<Response> {
    let limit = match query.limit.as_deref() {
        None => DEFAULT_LIMIT,
        Some(raw) => match raw.parse::<usize>() {
            Ok(n) if (1..=MAX_COMMANDS).contains(&n) => n,
            _ => {
                return Ok(error_response(
                    StatusCode::BAD_REQUEST,
                    &format!("limit must be an integer between 1 and {MAX_COMMANDS}"),
                ));
            }
        },
    };
    let all = match super::query_flag("all", query.all.as_deref()) {
        Ok(all) => all,
        Err(message) => return Ok(error_response(StatusCode::BAD_REQUEST, &message)),
    };
    let hub = Arc::clone(&app.hub);
    let list = blocking(move || hub.commands(&id, CommandFilter { all, limit })).await?;
    Ok(Json(list).into_response())
}

/// Default `limit` of the replies route.
pub const DEFAULT_REPLIES: usize = 20;

#[derive(Deserialize)]
pub(super) struct RepliesQuery {
    /// Parsed by hand so a bad value gets the API's JSON error.
    limit: Option<String>,
}

/// A session's replies, newest first, each with its Markdown blocks: at
/// most `limit` (1..=100, default 20).
pub(super) async fn list_replies(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiQuery(query): ApiQuery<RepliesQuery>,
) -> ApiResult<Response> {
    let limit = match query.limit.as_deref() {
        None => DEFAULT_REPLIES,
        Some(raw) => match raw.parse::<usize>() {
            Ok(n) if (1..=MAX_REPLIES).contains(&n) => n,
            _ => {
                return Ok(error_response(
                    StatusCode::BAD_REQUEST,
                    &format!("limit must be an integer between 1 and {MAX_REPLIES}"),
                ));
            }
        },
    };
    let hub = Arc::clone(&app.hub);
    let list = blocking(move || hub.replies(&id, limit)).await?;
    Ok(Json(list).into_response())
}

#[derive(Deserialize)]
pub(super) struct PasteRequest {
    text: String,
    /// Press Enter after the paste.
    #[serde(default)]
    run: bool,
}

/// Paste text into a running session's terminal (bracketed paste when the
/// program enabled it; Enter only with `run`).
pub(super) async fn paste(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(request): ApiJson<PasteRequest>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let outcome = blocking(move || hub.paste(&id, &request.text, request.run)).await?;
    Ok(Json(outcome).into_response())
}

/// The events socket's message for commands an agent session just
/// suggested.
pub(super) fn event_message(session_id: &str, mut added: Vec<Command>) -> Message {
    added.reverse();
    let fresh = added.first().is_some_and(is_fresh);
    text(json!({
        "type": "commands",
        "session_id": session_id,
        "fresh": fresh,
        "commands": added,
    }))
}

/// The events socket's message for replies an agent session just wrote
/// (or a fallback replaced by its transcript line: same `seq`), newest
/// first.
pub(super) fn replies_message(session_id: &str, added: Vec<AgentReply>) -> Message {
    let replies: Vec<ReplyView> = added.into_iter().rev().map(ReplyView::from).collect();
    text(json!({
        "type": "replies",
        "session_id": session_id,
        "replies": replies,
    }))
}
