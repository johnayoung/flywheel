//! Review from anywhere: `GET /api/sessions/{id}/review`, a file's diff
//! (`GET .../review/diff`), and the guarded actions (`POST
//! .../review/{commit,push,pr,rebase,abort,merge}`: `dry_run` plans, `confirm`
//! runs the plan with that fingerprint) and `POST .../review/ask`.

use super::ApiQuery;
use super::{ApiJson, ApiResult, AppState, blocking, error_response};
use crate::hub::HubError;
use crate::review::{Action, AskKind, PrRequest, Scope};
use crate::state::StateError;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Deserialize)]
pub(super) struct ReviewQuery {
    base: Option<String>,
}

/// The session's review: branch vs base, commits, files, working tree.
pub(super) async fn review(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiQuery(query): ApiQuery<ReviewQuery>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let tools = app.options.review_tools.clone();
    let view = blocking(move || hub.review(&tools, &id, query.base.as_deref())).await?;
    Ok(Json(view).into_response())
}

#[derive(Deserialize)]
pub(super) struct DiffQuery {
    scope: Option<String>,
    path: Option<String>,
    old_path: Option<String>,
    base: Option<String>,
    offset: Option<usize>,
}

/// One page of a file's diff.
pub(super) async fn diff(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiQuery(query): ApiQuery<DiffQuery>,
) -> ApiResult<Response> {
    let scope = match Scope::parse(query.scope.as_deref().unwrap_or("committed")) {
        Ok(scope) => scope,
        Err(message) => return Ok(error_response(StatusCode::BAD_REQUEST, &message)),
    };
    let Some(path) = query.path.filter(|p| !p.is_empty()) else {
        return Ok(error_response(StatusCode::BAD_REQUEST, "path is required"));
    };
    let hub = Arc::clone(&app.hub);
    let tools = app.options.review_tools.clone();
    let page = blocking(move || {
        hub.review_diff(
            &tools,
            &id,
            query.base.as_deref(),
            scope,
            &path,
            query.old_path.as_deref(),
            query.offset.unwrap_or(0),
        )
    })
    .await?;
    Ok(Json(page).into_response())
}

/// Every action's body: `dry_run` (plan only) or `confirm` (the plan's
/// fingerprint), plus what the action takes.
#[derive(Deserialize, Default)]
pub(super) struct ActionBody {
    #[serde(default)]
    dry_run: bool,
    confirm: Option<String>,
    message: Option<String>,
    /// Commit: the changed paths left out (absent: what looks generated
    /// or secret).
    exclude: Option<Vec<String>>,
    remote: Option<String>,
    title: Option<String>,
    body: Option<String>,
    base: Option<String>,
    #[serde(default)]
    draft: bool,
}

fn action(name: &str, body: &ActionBody) -> Action {
    match name {
        "commit" => Action::Commit {
            message: body.message.clone().unwrap_or_default(),
            exclude: body.exclude.clone(),
        },
        "push" => Action::Push {
            remote: body.remote.clone(),
        },
        "pr" => Action::Pr(PrRequest {
            title: body.title.clone(),
            body: body.body.clone(),
            base: body.base.clone(),
            draft: body.draft,
        }),
        "rebase" => Action::Rebase,
        "abort" => Action::Abort,
        _ => Action::Merge,
    }
}

async fn run_action(
    app: AppState,
    id: String,
    name: &'static str,
    body: ActionBody,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let tools = app.options.review_tools.clone();
    let action = action(name, &body);
    if body.dry_run {
        let plan = blocking(move || hub.review_plan(&tools, &id, &action)).await?;
        return Ok(Json(plan).into_response());
    }
    let Some(confirm) = body.confirm else {
        return Err(HubError::from(StateError::Invalid(
            "pass dry_run to see the plan, then confirm with its fingerprint".into(),
        ))
        .into());
    };
    let report = blocking(move || hub.review_run(&tools, &id, &action, &confirm)).await?;
    Ok(Json(report).into_response())
}

macro_rules! action_route {
    ($fn:ident, $name:literal) => {
        pub(super) async fn $fn(
            State(app): State<AppState>,
            Path(id): Path<String>,
            ApiJson(body): ApiJson<ActionBody>,
        ) -> ApiResult<Response> {
            run_action(app, id, $name, body).await
        }
    };
}

action_route!(commit, "commit");
action_route!(push, "push");
action_route!(pr, "pr");
action_route!(rebase, "rebase");
action_route!(abort, "abort");
action_route!(merge, "merge");

#[derive(Deserialize)]
pub(super) struct AskBody {
    kind: AskKind,
    #[serde(default)]
    dry_run: bool,
}

/// Paste a prompt into the session's agent (or, with `dry_run`, say which
/// prompt and where).
pub(super) async fn ask(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<AskBody>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let tools = app.options.review_tools.clone();
    let plan = blocking(move || hub.review_ask(&tools, &id, body.kind, !body.dry_run)).await?;
    Ok(Json(plan).into_response())
}
