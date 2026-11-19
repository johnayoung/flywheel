//! Watches over HTTP (see [`crate::watch`]): `GET /api/watches` (every
//! watch, or `?session=` one owner's), `POST /api/watches` (arm one), and
//! `DELETE /api/watches/{id}` (disarm it).

use super::{ApiJson, ApiQuery, ApiResult, AppState, blocking};
use crate::watch::NewWatch;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

#[derive(Deserialize, Default)]
pub(super) struct WatchFilter {
    session: Option<String>,
}

pub(super) async fn list_watches(
    State(app): State<AppState>,
    ApiQuery(filter): ApiQuery<WatchFilter>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let watches = blocking(move || {
        let owner = match filter.session.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(session) => Some(hub.session_id_named(session)?),
            None => None,
        };
        Ok(hub.watches(owner.as_deref()))
    })
    .await?;
    Ok(Json(json!({ "watches": watches })).into_response())
}

pub(super) async fn add_watch(
    State(app): State<AppState>,
    ApiJson(new): ApiJson<NewWatch>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let watch = blocking(move || hub.watch_add(&new, None)).await?;
    Ok((StatusCode::CREATED, Json(watch)).into_response())
}

pub(super) async fn remove_watch(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let removed = blocking(move || {
        let watch = hub.watch_named(&id)?;
        hub.watch_remove(&watch.id)
    })
    .await?;
    Ok(Json(removed).into_response())
}
