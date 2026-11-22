//! The build sweep over HTTP (see [`crate::sweep`]): `POST /api/sweep`
//! `{"all"?, "dry_run"?}` -- the candidates, the worktrees kept and why,
//! and (unless a dry run) what was deleted.

use super::{ApiJson, ApiResult, AppState, blocking};
use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SweepBody {
    #[serde(default)]
    all: bool,
    #[serde(default)]
    dry_run: bool,
}

pub(super) async fn sweep(
    State(app): State<AppState>,
    ApiJson(body): ApiJson<SweepBody>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let report = blocking(move || Ok(hub.sweep(body.all, body.dry_run, "manual"))).await?;
    Ok(Json(report).into_response())
}
