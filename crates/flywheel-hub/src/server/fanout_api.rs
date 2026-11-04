//! Verified fan-outs (see `crate::fanout`):
//!
//! * `GET /api/fanouts` -- `{"enabled","runner","fanouts":[view...]}`,
//!   newest first; `POST` launches one (201 its view);
//! * `GET /api/fanouts/{id}` -- the ranked board; `DELETE` forgets it;
//! * `POST /api/fanouts/{id}/grade {"candidate"}` -- grade again now;
//! * `POST .../fix {"candidate","dry_run"?}` -- paste what failed into the
//!   candidate's agent (dry run: the prompt);
//! * `POST .../pick {"candidate","dry_run"|"confirm"}` -- merge the winner
//!   through the review merge path (dry run: the plan and its fingerprint);
//! * `POST .../rebase {"candidate","dry_run"|"confirm"}` -- rebase onto
//!   the base (it is graded again);
//! * `POST .../cleanup {"candidates"?,"remove_worktrees"?,"force"?,"dry_run"?}`;
//! * `POST .../seen {"graded_at"}` -- that grading was looked at: the
//!   fan-out leaves Needs you and its workspace's done count;
//! * `POST .../promote` -- write it as a flywheel task (`promote_api`);
//! * `{"type":"fanout","id"}` on the events socket when one changes.

use super::{ApiJson, ApiResult, AppState, blocking, error_response};
use crate::fanout::{CleanupRequest, CreateFanout, Fanouts};
use crate::hub::HubError;
use crate::state::StateError;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

fn engine(app: &AppState) -> Option<Arc<Fanouts>> {
    app.options.fanouts.clone()
}

fn off() -> Response {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "fan-outs are off on this hub (it was started without them)",
    )
}

/// Every fan-out, newest first.
pub(super) async fn list(State(app): State<AppState>) -> ApiResult<Response> {
    let Some(engine) = app.options.fanouts.clone() else {
        return Ok(
            Json(json!({ "enabled": false, "runner": null, "fanouts": [] })).into_response(),
        );
    };
    let runner = engine.runner_name();
    let fanouts = blocking(move || engine.list()).await?;
    Ok(Json(json!({ "enabled": true, "runner": runner, "fanouts": fanouts })).into_response())
}

/// Launch a fan-out.
pub(super) async fn create(
    State(app): State<AppState>,
    ApiJson(body): ApiJson<CreateFanout>,
) -> ApiResult<Response> {
    let Some(engine) = engine(&app) else {
        return Ok(off());
    };
    // "Fan out this task": the task and its held-out graders, read now.
    let body = super::promote_api::seed_request(&app, body).await?;
    let view = blocking(move || engine.create(body)).await?;
    Ok((StatusCode::CREATED, Json(view)).into_response())
}

pub(super) async fn get_one(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let Some(engine) = engine(&app) else {
        return Ok(off());
    };
    let view = blocking(move || engine.view(&id)).await?;
    Ok(Json(view).into_response())
}

pub(super) async fn remove(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let Some(engine) = engine(&app) else {
        return Ok(off());
    };
    blocking(move || engine.remove(&id)).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
pub(super) struct SeenBody {
    graded_at: chrono::DateTime<chrono::Utc>,
}

/// The board's grading of `graded_at` was looked at.
pub(super) async fn seen(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<SeenBody>,
) -> ApiResult<Response> {
    let Some(engine) = engine(&app) else {
        return Ok(off());
    };
    blocking(move || engine.seen(&id, body.graded_at)).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
pub(super) struct CandidateBody {
    candidate: String,
    #[serde(default)]
    dry_run: bool,
    confirm: Option<String>,
    /// Pick: land what looks like a secret, personal settings, or build
    /// output anyway.
    #[serde(default)]
    include_suspects: bool,
}

pub(super) async fn grade(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<CandidateBody>,
) -> ApiResult<Response> {
    let Some(engine) = engine(&app) else {
        return Ok(off());
    };
    let view = blocking(move || engine.grade(&id, &body.candidate)).await?;
    Ok((StatusCode::ACCEPTED, Json(view)).into_response())
}

pub(super) async fn fix(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<CandidateBody>,
) -> ApiResult<Response> {
    let Some(engine) = engine(&app) else {
        return Ok(off());
    };
    let plan = blocking(move || engine.fix(&id, &body.candidate, !body.dry_run)).await?;
    Ok(Json(plan).into_response())
}

fn needs_confirm() -> HubError {
    StateError::Invalid("pass dry_run to see the plan, then confirm with its fingerprint".into())
        .into()
}

pub(super) async fn pick(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<CandidateBody>,
) -> ApiResult<Response> {
    let Some(engine) = engine(&app) else {
        return Ok(off());
    };
    if body.dry_run {
        let plan =
            blocking(move || engine.pick_plan(&id, &body.candidate, body.include_suspects)).await?;
        return Ok(Json(plan).into_response());
    }
    let Some(confirm) = body.confirm else {
        return Err(needs_confirm().into());
    };
    let report =
        blocking(move || engine.pick(&id, &body.candidate, &confirm, body.include_suspects))
            .await?;
    Ok(Json(report).into_response())
}

pub(super) async fn rebase(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<CandidateBody>,
) -> ApiResult<Response> {
    let Some(engine) = engine(&app) else {
        return Ok(off());
    };
    if !body.dry_run && body.confirm.is_none() {
        return Err(needs_confirm().into());
    }
    let confirm = if body.dry_run { None } else { body.confirm };
    let report = blocking(move || engine.rebase(&id, &body.candidate, confirm.as_deref())).await?;
    Ok(Json(report).into_response())
}

pub(super) async fn cleanup(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<CleanupRequest>,
) -> ApiResult<Response> {
    let Some(engine) = engine(&app) else {
        return Ok(off());
    };
    let report = blocking(move || engine.cleanup(&id, &body)).await?;
    Ok(Json(report).into_response())
}
