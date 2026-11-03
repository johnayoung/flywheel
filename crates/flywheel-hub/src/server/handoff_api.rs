//! Cross-agent handoff and review pairing (see [`crate::agent_handoff`]):
//! `POST /api/sessions/{id}/handoff/compose` (on the source's hub),
//! `POST /api/sessions/{id}/handoff` (on the target's), `POST
//! .../review/reviewer` (ask for a review), `GET .../review/findings`,
//! and `POST .../review/findings/send`.

use super::{ApiJson, ApiResult, AppState, blocking};
use crate::agent_handoff::review::ReviewRequest;
use crate::agent_handoff::{ComposeRequest, Delivery};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use std::sync::Arc;

/// Compose the prompt handing part of session `id` to another agent.
pub(super) async fn compose(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(request): ApiJson<ComposeRequest>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let tools = app.options.review_tools.clone();
    let composed = blocking(move || hub.handoff_compose(&tools, &id, &request)).await?;
    Ok(Json(composed).into_response())
}

/// Paste a (composed, maybe edited) handoff into agent session `id`.
pub(super) async fn deliver(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(delivery): ApiJson<Delivery>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let delivered = blocking(move || hub.handoff_deliver(&id, &delivery)).await?;
    Ok(Json(delivered).into_response())
}

/// Launch (or ask again) a reviewer of session `id`'s work.
pub(super) async fn reviewer(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(request): ApiJson<ReviewRequest>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let tools = app.options.review_tools.clone();
    let dry = request.dry_run;
    let launched = blocking(move || hub.request_review(&tools, &id, &request)).await?;
    let status = if dry || launched.reused {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((status, Json(launched)).into_response())
}

/// The reviewer's findings so far.
pub(super) async fn findings(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let findings = blocking(move || hub.review_findings(&id)).await?;
    Ok(Json(findings).into_response())
}

#[derive(Deserialize, Default)]
pub(super) struct SendBody {
    /// The user's edit of the findings prompt (default: as composed).
    #[serde(default)]
    prompt: Option<String>,
}

/// Paste the findings into the author agent.
pub(super) async fn send_findings(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<SendBody>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let delivered = blocking(move || hub.send_findings(&id, body.prompt.as_deref())).await?;
    Ok(Json(delivered).into_response())
}
