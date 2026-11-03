//! Launch templates and usage: `GET /api/templates`,
//! `POST /api/workspaces/{id}/templates`,
//! `DELETE /api/workspaces/{id}/templates/{name}`, and `GET /api/usage`.

use super::{ApiJson, ApiResult, AppState, blocking};
use crate::agent::{AgentLaunch, SessionKind};
use crate::state::LaunchTemplate;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

#[derive(Deserialize)]
pub(super) struct TemplatesQuery {
    workspace_id: Option<String>,
}

/// Every workspace's templates, or one's (`workspace_id`).
pub(super) async fn list(
    State(app): State<AppState>,
    Query(query): Query<TemplatesQuery>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let templates = blocking(move || hub.templates(query.workspace_id.as_deref())).await?;
    Ok(Json(json!({ "templates": templates })).into_response())
}

/// A template as the API takes it.
#[derive(Deserialize)]
pub(super) struct TemplateBody {
    name: String,
    kind: SessionKind,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(flatten)]
    launch: AgentLaunch,
    #[serde(default)]
    worktree: bool,
}

/// Create a template, or replace the workspace's one of the same name.
pub(super) async fn save(
    State(app): State<AppState>,
    Path(workspace_id): Path<String>,
    ApiJson(body): ApiJson<TemplateBody>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let template = LaunchTemplate {
        name: body.name,
        kind: body.kind,
        prompt: body.prompt,
        launch: body.launch,
        worktree: body.worktree,
        updated_at: chrono::Utc::now(),
    };
    let saved = blocking(move || hub.save_template(&workspace_id, template)).await?;
    Ok(Json(saved).into_response())
}

pub(super) async fn remove(
    State(app): State<AppState>,
    Path((workspace_id, name)): Path<(String, String)>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    blocking(move || hub.remove_template(&workspace_id, &name)).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Tokens and cost: the hub's and each workspace's, today, this week,
/// and all time.
pub(super) async fn usage(State(app): State<AppState>) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let usage = blocking(move || Ok(hub.usage_rollups())).await?;
    Ok(Json(usage).into_response())
}
