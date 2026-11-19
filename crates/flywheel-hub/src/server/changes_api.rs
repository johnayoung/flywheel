//! Adopted worktrees over HTTP (see [`crate::changes`]): `GET
//! /api/changes[?untracked=1]` (tracked changes with their counts; with
//! `untracked`, also the workspaces' worktrees nothing tracks), `POST
//! /api/changes/adopt` `{"path","base"?,"session"?}` (201 new, 200
//! tracked already), and `POST /api/changes/{id}/close`
//! `{"worktree"?,"build"?,"report"?,"dry_run"?}`.

use super::{ApiJson, ApiQuery, ApiResult, AppState, blocking};
use crate::close::CloseChoices;
use crate::hub::changes::Adopt;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

#[derive(Deserialize, Default)]
pub(super) struct ListQuery {
    untracked: Option<String>,
}

pub(super) async fn list_changes(
    State(app): State<AppState>,
    ApiQuery(query): ApiQuery<ListQuery>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let untracked = matches!(query.untracked.as_deref(), Some("1" | "true"));
    let view = blocking(move || {
        let changes = hub.changes(true);
        let others: Vec<serde_json::Value> = if untracked {
            hub.untracked_worktrees()
                .into_iter()
                .map(|(repo, w)| json!({ "repo": repo, "path": w.path, "branch": w.branch, "head": w.head }))
                .collect()
        } else {
            Vec::new()
        };
        Ok(json!({ "changes": changes, "untracked": others }))
    })
    .await?;
    Ok(Json(view).into_response())
}

pub(super) async fn adopt(
    State(app): State<AppState>,
    ApiJson(adopt): ApiJson<Adopt>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let (change, new) = blocking(move || hub.adopt_worktree(None, &adopt)).await?;
    let status = if new {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(change)).into_response())
}

#[derive(Deserialize)]
pub(super) struct CloseBody {
    #[serde(flatten)]
    choices: CloseChoices,
    #[serde(default)]
    dry_run: bool,
}

pub(super) async fn close_change(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<CloseBody>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let done = blocking(move || hub.close_change(&id, &body.choices, body.dry_run)).await?;
    Ok(Json(done).into_response())
}
