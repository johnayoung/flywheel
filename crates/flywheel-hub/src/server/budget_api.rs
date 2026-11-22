//! The build budget over HTTP (see [`crate::budget`]): `GET /api/budget`,
//! the pool (slots in use, by session), the machine's pressure and what
//! bounds it, and every other hub's last sample.

use super::{ApiResult, AppState, blocking};
use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

pub(super) async fn get_budget(State(app): State<AppState>) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let view = blocking(move || Ok(hub.budget_view())).await?;
    Ok(Json(view).into_response())
}
