//! Broadcast (see [`crate::broadcast`]): `POST /api/broadcast`
//! `{"targets","text","header"?}` answers every target's result.

use super::{ApiJson, ApiResult, AppState, blocking};
use crate::broadcast::BroadcastRequest;
use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

/// Send one prompt to several agent sessions: each typed in now, queued,
/// or refused (with why).
pub(super) async fn broadcast(
    State(app): State<AppState>,
    ApiJson(request): ApiJson<BroadcastRequest>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let done = blocking(move || hub.broadcast(&request)).await?;
    Ok(Json(done).into_response())
}
