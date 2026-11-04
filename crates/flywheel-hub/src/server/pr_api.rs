//! Pull requests and CI (`crate::pr`): `GET /api/sessions/{id}/pr`
//! (`?refresh=1` asks GitHub now), a failing check's log (`GET .../pr/log
//! ?check=`), asking the agent to fix CI (`POST .../pr/fix`), and the
//! guarded actions (`POST .../pr/{rerun,ready,merge}`: `dry_run` plans,
//! `confirm` runs the plan with that fingerprint; merge's `force` is the
//! typed-back pull request number a forcible refusal needs).

use super::ApiQuery;
use super::{ApiJson, ApiResult, AppState, blocking};
use crate::hub::HubError;
use crate::pr::PrAction;
use crate::state::StateError;
use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Deserialize)]
pub(super) struct ViewQuery {
    #[serde(default)]
    refresh: Option<String>,
}

pub(super) async fn view(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiQuery(query): ApiQuery<ViewQuery>,
) -> ApiResult<Response> {
    let refresh = matches!(query.refresh.as_deref(), Some("1" | "true" | "yes"));
    let hub = Arc::clone(&app.hub);
    let view = blocking(move || hub.pr_view(&id, refresh)).await?;
    Ok(Json(view).into_response())
}

#[derive(Deserialize)]
pub(super) struct LogQuery {
    check: Option<String>,
}

pub(super) async fn log(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiQuery(query): ApiQuery<LogQuery>,
) -> ApiResult<Response> {
    let Some(check) = query.check.filter(|c| !c.trim().is_empty()) else {
        return Err(HubError::from(StateError::Invalid("check is required".into())).into());
    };
    let hub = Arc::clone(&app.hub);
    let view = blocking(move || hub.pr_log(&id, &check)).await?;
    Ok(Json(view).into_response())
}

#[derive(Deserialize, Default)]
pub(super) struct FixBody {
    check: Option<String>,
    #[serde(default)]
    dry_run: bool,
}

pub(super) async fn fix(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<FixBody>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let report = blocking(move || hub.pr_fix(&id, body.check.as_deref(), !body.dry_run)).await?;
    Ok(Json(report).into_response())
}

#[derive(Deserialize, Default)]
pub(super) struct ActionBody {
    #[serde(default)]
    dry_run: bool,
    confirm: Option<String>,
    /// Merge: the pull request's number, typed back to override checks
    /// failing or a review missing.
    force: Option<String>,
}

async fn run(app: AppState, id: String, action: PrAction, body: ActionBody) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    if body.dry_run {
        let plan = blocking(move || hub.pr_plan(&id, action)).await?;
        return Ok(Json(plan).into_response());
    }
    let Some(confirm) = body.confirm else {
        return Err(HubError::from(StateError::Invalid(
            "pass dry_run to see the plan, then confirm with its fingerprint".into(),
        ))
        .into());
    };
    let outcome =
        blocking(move || hub.pr_run(&id, action, &confirm, body.force.as_deref())).await?;
    Ok(Json(outcome).into_response())
}

macro_rules! action_route {
    ($fn:ident, $action:expr) => {
        pub(super) async fn $fn(
            State(app): State<AppState>,
            Path(id): Path<String>,
            ApiJson(body): ApiJson<ActionBody>,
        ) -> ApiResult<Response> {
            run(app, id, $action, body).await
        }
    };
}

action_route!(rerun, PrAction::Rerun);
action_route!(ready, PrAction::Ready);
action_route!(merge, PrAction::Merge);
