//! Review records over HTTP (see [`crate::reviews`]): `GET
//! /api/verdicts` (`?repo=&sha=&reviewer=&since=&limit=`, newest first),
//! `POST /api/verdicts` (record one as the operator), `GET
//! /api/verdicts/coverage?repo=&range=BASE..HEAD[&patch_equivalent=1]`,
//! and `POST /api/verdicts/{id}/retract` `{"reason"}`.

use super::{ApiJson, ApiQuery, ApiResult, AppState, blocking};
use crate::hub::reviews::ReviewInput;
use crate::reviews::ReviewQuery;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

pub(super) async fn list_verdicts(
    State(app): State<AppState>,
    ApiQuery(query): ApiQuery<ReviewQuery>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let reviews = blocking(move || hub.reviews(&query)).await?;
    Ok(Json(json!({ "reviews": reviews })).into_response())
}

pub(super) async fn record_verdict(
    State(app): State<AppState>,
    ApiJson(input): ApiJson<ReviewInput>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let review = blocking(move || hub.record_review(None, &input)).await?;
    Ok((StatusCode::CREATED, Json(review)).into_response())
}

#[derive(Deserialize, Default)]
pub(super) struct CoverageQuery {
    repo: String,
    range: String,
    #[serde(default)]
    patch_equivalent: Option<String>,
}

pub(super) async fn coverage(
    State(app): State<AppState>,
    ApiQuery(query): ApiQuery<CoverageQuery>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let twins = matches!(query.patch_equivalent.as_deref(), Some("1" | "true"));
    let coverage =
        blocking(move || hub.check_reviewed(None, &query.repo, &query.range, twins)).await?;
    Ok(Json(coverage).into_response())
}

#[derive(Deserialize)]
pub(super) struct Retract {
    reason: String,
}

pub(super) async fn retract_verdict(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<Retract>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let review = blocking(move || hub.retract_review(&id, &body.reason)).await?;
    Ok(Json(review).into_response())
}
