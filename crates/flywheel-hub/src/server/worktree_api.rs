//! Worktrees and companion shells: `DELETE /api/sessions/{id}` with
//! `remove_worktree`, `GET /api/sessions/{id}/worktree`,
//! `POST /api/sessions/{id}/companion`, and the new-session form's
//! `GET /api/worktrees/plan`.

use super::{ApiJson, ApiResult, AppState, blocking, error_response};
use crate::hub::RemoveOptions;
use crate::state::expand_home;
use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;

/// A boolean query flag: absent or `false`/`0` is false; `true`/`1`/empty
/// is true; anything else is an error naming `name`.
fn flag(raw: Option<&str>, name: &str) -> Result<bool, String> {
    match raw {
        None | Some("false" | "0") => Ok(false),
        Some("true" | "1" | "") => Ok(true),
        Some(other) => Err(format!("{name} must be true or false, not {other:?}")),
    }
}

#[derive(Deserialize)]
pub(super) struct RemoveQuery {
    remove_worktree: Option<String>,
    force: Option<String>,
}

/// Kill and forget a session: 204. With `remove_worktree=true` its
/// companion shells and worktree go too (`force=true` even when that
/// loses work): 200 with what was removed.
pub(super) async fn remove_session(
    State(app): State<AppState>,
    Path(id): Path<String>,
    query: Result<Query<RemoveQuery>, QueryRejection>,
) -> ApiResult<Response> {
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return Ok(error_response(rejection.status(), &rejection.body_text())),
    };
    let options = match (
        flag(query.remove_worktree.as_deref(), "remove_worktree"),
        flag(query.force.as_deref(), "force"),
    ) {
        (Ok(remove_worktree), Ok(force)) => RemoveOptions {
            remove_worktree,
            force,
        },
        (Err(message), _) | (_, Err(message)) => {
            return Ok(error_response(StatusCode::BAD_REQUEST, &message));
        }
    };
    let hub = Arc::clone(&app.hub);
    let outcome = blocking(move || hub.remove_session_with(&id, options)).await?;
    if options.remove_worktree {
        Ok(Json(outcome).into_response())
    } else {
        Ok(StatusCode::NO_CONTENT.into_response())
    }
}

/// A worktree session's worktree and what removing it would lose.
pub(super) async fn worktree_info(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let info = blocking(move || hub.worktree_info(&id)).await?;
    Ok(Json(info).into_response())
}

#[derive(Deserialize, Default)]
pub(super) struct CompanionRequest {
    cols: Option<u16>,
    rows: Option<u16>,
    /// Paste this once the shell is ready.
    text: Option<String>,
    /// Press Enter after the paste.
    #[serde(default)]
    run: bool,
}

/// An agent session's companion shell, started on demand (201 when
/// started now, 200 when it ran already). With `text`, also paste it
/// (waiting for a new shell's prompt): the session, whether it was
/// started, how it was judged ready, and the paste.
pub(super) async fn companion(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(request): ApiJson<CompanionRequest>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    match request.text {
        Some(text) => {
            let outcome = blocking(move || {
                hub.send_to_companion(&id, &text, request.run, request.cols, request.rows)
            })
            .await?;
            let status = if outcome.created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            Ok((status, Json(outcome)).into_response())
        }
        None => {
            let (view, created) =
                blocking(move || hub.companion(&id, request.cols, request.rows)).await?;
            let status = if created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            Ok((status, Json(view)).into_response())
        }
    }
}

#[derive(Deserialize)]
pub(super) struct PlanQuery {
    folder: Option<String>,
    task: Option<String>,
    base: Option<String>,
}

/// Where a folder's worktrees go, its branches and default base, and --
/// with `task` -- the worktree that would be cut, or why not. Always 200
/// once `folder` is given (the form checks as you type).
pub(super) async fn plan(
    State(app): State<AppState>,
    query: Result<Query<PlanQuery>, QueryRejection>,
) -> ApiResult<Response> {
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return Ok(error_response(rejection.status(), &rejection.body_text())),
    };
    let Some(folder) = query.folder.filter(|f| !f.trim().is_empty()) else {
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            "folder is required",
        ));
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let folder = expand_home(&PathBuf::from(folder.trim()), home.as_deref());
    let hub = Arc::clone(&app.hub);
    let view = blocking(move || {
        Ok(hub.worktree_plan(&folder, query.task.as_deref(), query.base.as_deref()))
    })
    .await?;
    Ok(Json(view).into_response())
}
