//! A folder's past agent conversations, to resume in a new session; the
//! Claude accounts a new session can launch with.

use super::ApiQuery;
use super::{ApiJson, ApiResult, AppState, blocking, error_response};
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

#[derive(Deserialize)]
pub(super) struct AccountBody {
    name: String,
    token: String,
    #[serde(default)]
    replace: bool,
}

fn account_error(e: &crate::accounts::AccountError) -> Response {
    use crate::accounts::AccountError;
    let status = match e {
        AccountError::Invalid(_) => StatusCode::BAD_REQUEST,
        AccountError::NotFound(_) => StatusCode::NOT_FOUND,
        AccountError::Exists(_) => StatusCode::CONFLICT,
        AccountError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    error_response(status, &e.to_string())
}

/// Store a `claude setup-token` token as an account (`"replace":true`
/// changes an existing one's): `{"name","updated"}`, never the token.
pub(super) async fn add_account(
    State(app): State<AppState>,
    ApiJson(body): ApiJson<AccountBody>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let name = body.name.trim().to_owned();
    let result = blocking(move || {
        Ok(
            crate::accounts::add(hub.state_dir(), &name, &body.token, body.replace).map(|()| {
                crate::accounts::list(hub.state_dir())
                    .into_iter()
                    .find(|a| a.name == name)
            }),
        )
    })
    .await?;
    Ok(match result {
        Ok(account) => (StatusCode::CREATED, Json(json!(account))).into_response(),
        Err(e) => account_error(&e),
    })
}

/// Delete an account (its sessions fail to relaunch until it is back).
pub(super) async fn remove_account(
    State(app): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let result = blocking(move || Ok(crate::accounts::remove(hub.state_dir(), &name))).await?;
    Ok(match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => account_error(&e),
    })
}
