//! Prompt queues (see [`crate::queue`]): `GET /api/queues`, `GET|POST
//! /api/sessions/{id}/queue`, `POST .../queue/clear|pause|resume`,
//! `PATCH|DELETE .../queue/items/{item}`, `POST
//! .../queue/items/{item}/move|editing`.

use super::{ApiJson, ApiResult, AppState, blocking};
use crate::queue::{ItemEdit, NewItem};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

/// Every session's queue.
pub(super) async fn list(State(app): State<AppState>) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let views = blocking(move || Ok(hub.queue_views())).await?;
    Ok(Json(views).into_response())
}

/// Session `id`'s queue.
pub(super) async fn get_queue(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let view = blocking(move || hub.queue_view(&id)).await?;
    Ok(Json(view).into_response())
}

/// Queue a prompt: `{"text","when"?}`. Answers `{"item","queue"}`.
pub(super) async fn add(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(item): ApiJson<NewItem>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let (item, queue) = blocking(move || hub.queue_add(&id, &item)).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "item": item, "queue": queue })),
    )
        .into_response())
}

#[derive(Deserialize, Default)]
pub(super) struct ClearRequest {
    /// Also forget the delivered items.
    #[serde(default)]
    all: bool,
}

pub(super) async fn clear(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(request): ApiJson<ClearRequest>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let view = blocking(move || hub.queue_clear(&id, request.all)).await?;
    Ok(Json(view).into_response())
}

pub(super) async fn pause(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let view = blocking(move || hub.queue_set_paused(&id, true)).await?;
    Ok(Json(view).into_response())
}

pub(super) async fn resume(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let view = blocking(move || hub.queue_set_paused(&id, false)).await?;
    Ok(Json(view).into_response())
}

/// Edit a pending item: `{"text"?,"when"?}`.
pub(super) async fn edit(
    State(app): State<AppState>,
    Path((id, item)): Path<(String, String)>,
    ApiJson(edit): ApiJson<ItemEdit>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let view = blocking(move || hub.queue_edit(&id, &item, &edit)).await?;
    Ok(Json(view).into_response())
}

pub(super) async fn remove(
    State(app): State<AppState>,
    Path((id, item)): Path<(String, String)>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let view = blocking(move || hub.queue_remove(&id, &item)).await?;
    Ok(Json(view).into_response())
}

#[derive(Deserialize)]
pub(super) struct MoveRequest {
    /// 1-based place among the pending items.
    position: usize,
}

pub(super) async fn move_item(
    State(app): State<AppState>,
    Path((id, item)): Path<(String, String)>,
    ApiJson(request): ApiJson<MoveRequest>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let view = blocking(move || hub.queue_move(&id, &item, request.position)).await?;
    Ok(Json(view).into_response())
}

#[derive(Deserialize)]
pub(super) struct EditingRequest {
    /// An editor has the item open (renew within the lease), or closed it.
    editing: bool,
}

/// Keep a pending item back while an editor has it open:
/// `{"editing":true}` (renewed by the editor; it lapses on its own), then
/// `{"editing":false}`. Answers 204.
pub(super) async fn editing(
    State(app): State<AppState>,
    Path((id, item)): Path<(String, String)>,
    ApiJson(request): ApiJson<EditingRequest>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    blocking(move || hub.queue_editing(&id, &item, request.editing)).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
