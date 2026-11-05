//! A folder's past agent conversations, to resume in a new session; the
//! Claude accounts a new session can launch with.

use super::ApiQuery;
use super::{ApiResult, AppState, blocking, error_response};
use crate::agent::SessionKind;
use crate::conversations::{DEFAULT_LIMIT, MAX_LIMIT};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Deserialize)]
pub(super) struct ConversationsQuery {
    kind: SessionKind,
    #[serde(default)]
    folder: Option<PathBuf>,
    #[serde(default)]
    limit: Option<usize>,
}

/// `{"conversations": [{"id","kind","modified","title"?,"prompt"?,
/// "bytes","open_in"?}]}`, newest first.
pub(super) async fn list(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiQuery(query): ApiQuery<ConversationsQuery>,
) -> ApiResult<Response> {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            &format!("limit must be between 1 and {MAX_LIMIT}"),
        ));
    }
    let hub = Arc::clone(&app.hub);
    let list = blocking(move || hub.conversations(&id, query.folder.as_deref(), query.kind, limit))
        .await?;
    Ok(Json(json!({ "conversations": list })).into_response())
}

/// `{"accounts": [{"name","updated"}]}`: names only, never tokens.
pub(super) async fn accounts(State(app): State<AppState>) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let list = blocking(move || Ok(crate::accounts::list(hub.state_dir()))).await?;
    Ok(Json(json!({ "accounts": list })).into_response())
}
