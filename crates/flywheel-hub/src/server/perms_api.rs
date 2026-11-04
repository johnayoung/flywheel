//! Agent permission rules over HTTP (see [`crate::perms`]): `GET
//! /api/perms` (every source for a folder or session, and a blocked
//! session's pending request with its proposal), `GET /api/perms/suggest`,
//! `POST /api/perms/preview`, and `POST /api/perms/allow` (confirm-gated;
//! dangerous rules typed back). The hub's token or a login only: peers
//! are refused (`peers::peer_forbidden`).

use super::{ApiJson, ApiQuery, ApiResult, AppState, blocking};
use crate::perms::{AllowBody, Where};
use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

pub(super) async fn view(
    State(app): State<AppState>,
    ApiQuery(at): ApiQuery<Where>,
) -> ApiResult<Response> {
    let hub = app.hub.clone();
    let view = blocking(move || hub.perms_view(&at)).await?;
    Ok(Json(view).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SuggestQuery {
    folder: Option<String>,
    session: Option<String>,
    /// Look back this many days (default 14).
    days: Option<i64>,
}

pub(super) async fn suggest(
    State(app): State<AppState>,
    ApiQuery(query): ApiQuery<SuggestQuery>,
) -> ApiResult<Response> {
    let hub = app.hub.clone();
    let at = Where {
        folder: query.folder,
        session: query.session,
    };
    let days = query.days.unwrap_or(14);
    let found = blocking(move || hub.perms_suggest(&at, days)).await?;
    Ok(Json(found).into_response())
}

pub(super) async fn preview(
    State(app): State<AppState>,
    ApiJson(body): ApiJson<AllowBody>,
) -> ApiResult<Response> {
    let hub = app.hub.clone();
    let preview = blocking(move || hub.perms_preview(&body)).await?;
    Ok(Json(preview).into_response())
}

pub(super) async fn allow(
    State(app): State<AppState>,
    ApiJson(body): ApiJson<AllowBody>,
) -> ApiResult<Response> {
    let hub = app.hub.clone();
    let done = blocking(move || hub.perms_allow(&body)).await?;
    Ok(Json(done).into_response())
}
