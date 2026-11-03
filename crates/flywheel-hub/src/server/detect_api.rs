//! `GET /api/sessions/{id}/explain`: why a session shows its status (what
//! set it, and its live screen read with its kind's detection manifest).

use super::{ApiResult, AppState, blocking};
use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};

pub(super) async fn explain(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let hub = app.hub.clone();
    let explanation = blocking(move || hub.explain(&id)).await?;
    Ok(Json(explanation).into_response())
}
