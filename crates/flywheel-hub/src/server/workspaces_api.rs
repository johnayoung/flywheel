//! Workspaces: `/api/workspaces` (list, get, create, update, delete) and
//! a workspace's layout (`GET`/`PUT /api/workspaces/{id}/layout`).

use super::{ApiError, ApiJson, ApiResult, AppState, blocking};
use crate::hub::HubError;
use crate::state::{StateError, WorkspaceInput};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

// Both stat every folder (git/missing flags): off the executor.
pub(super) async fn list_workspaces(State(app): State<AppState>) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let (workspaces, templates) =
        blocking(move || Ok((hub.workspaces(), hub.templates(None)?))).await?;
    // Each workspace with its launch templates (the new-session form and
    // the palette's quick launch read them from here).
    let workspaces: Vec<serde_json::Value> = workspaces
        .into_iter()
        .map(|workspace| {
            let mut value = serde_json::to_value(&workspace).unwrap_or_default();
            let own: Vec<&crate::hub::TemplateView> = templates
                .iter()
                .filter(|t| t.workspace_id == workspace.id)
                .collect();
            value["templates"] = serde_json::to_value(own).unwrap_or_default();
            value
        })
        .collect();
    Ok(Json(json!({ "workspaces": workspaces })).into_response())
}

pub(super) async fn get_workspace(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    Ok(Json(blocking(move || hub.workspace(&id)).await?).into_response())
}

pub(super) async fn create_workspace(
    State(app): State<AppState>,
    ApiJson(input): ApiJson<WorkspaceInput>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let workspace = blocking(move || hub.create_workspace(input)).await?;
    Ok((StatusCode::CREATED, Json(workspace)).into_response())
}

pub(super) async fn update_workspace(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<WorkspaceInput>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let workspace = blocking(move || hub.update_workspace(&id, input)).await?;
    Ok(Json(workspace).into_response())
}

pub(super) async fn delete_workspace(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    blocking(move || hub.delete_workspace(&id)).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub(super) async fn get_layout(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let layout = app.hub.layout(&id)?;
    Ok(Json(json!({ "workspace_id": id, "rev": layout.rev, "tabs": layout.tabs })).into_response())
}

#[derive(Deserialize)]
pub(super) struct LayoutInput {
    rev: u64,
    tabs: Vec<crate::layout::Tab>,
}

/// Save a layout written against `rev`. A stale `rev` is a 409 carrying
/// the current layout (`{"error","layout"}`), so the client can rebase.
pub(super) async fn put_layout(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<LayoutInput>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let workspace = id.clone();
    let saved = blocking(move || hub.set_layout(&workspace, input.rev, input.tabs)).await;
    match saved {
        Ok(layout) => Ok(Json(
            json!({ "workspace_id": id, "rev": layout.rev, "tabs": layout.tabs }),
        )
        .into_response()),
        Err(ApiError(HubError::State(StateError::Conflict(message)))) => {
            let current = app.hub.layout(&id)?;
            Ok((
                StatusCode::CONFLICT,
                Json(json!({
                    "error": message,
                    "layout": { "workspace_id": id, "rev": current.rev, "tabs": current.tabs },
                })),
            )
                .into_response())
        }
        Err(e) => Err(e),
    }
}
