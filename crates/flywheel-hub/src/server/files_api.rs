//! `GET /api/sessions/{id}/file` and `POST /api/sessions/{id}/files/check`:
//! the files a session's output names, for the web viewer (see
//! [`crate::files`]).

use super::ApiQuery;
use super::{ApiJson, ApiResult, AppState, blocking, error_response};
use crate::files::{FileError, MAX_CHECK_PATHS, Scope};
use crate::hub::Hub;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

/// Where `id`'s paths resolve: its process's directory now, then the
/// roots -- the directory it was opened in, its worktree, its workspace's
/// folders.
fn scope(hub: &Hub, id: &str) -> Result<Scope, crate::hub::HubError> {
    let view = hub.session(id)?;
    let record = view.record;
    let mut roots = vec![record.cwd.clone()];
    if let Some(worktree) = &record.worktree {
        roots.push(worktree.path.clone());
    }
    if let Ok(workspace) = hub.workspace(&record.workspace_id) {
        roots.extend(workspace.folders.into_iter().map(|f| f.path));
    }
    roots.dedup();
    let now = view.live.and_then(|l| l.cwd);
    Ok(Scope {
        bases: now.into_iter().chain(roots.iter().cloned()).collect(),
        roots,
        home: crate::config::home_dir(),
    })
}

fn file_error(error: &FileError) -> Response {
    let status = match error {
        FileError::Invalid(_) | FileError::Directory(_) => StatusCode::BAD_REQUEST,
        FileError::NotFound(_) => StatusCode::NOT_FOUND,
        FileError::Outside(_) => StatusCode::FORBIDDEN,
        FileError::Binary(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
        FileError::Io(..) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    error_response(status, &error.to_string())
}

#[derive(Deserialize)]
pub(super) struct FileQuery {
    path: Option<String>,
}

/// A file under the session's folders: `{"path","size","text","truncated"}`.
pub(super) async fn read_file(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiQuery(query): ApiQuery<FileQuery>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let requested = query.path.unwrap_or_default();
    let result = blocking(move || Ok(scope(&hub, &id)?.read(&requested))).await?;
    Ok(match result {
        Ok(view) => Json(view).into_response(),
        Err(error) => file_error(&error),
    })
}

#[derive(Deserialize)]
pub(super) struct CheckRequest {
    paths: Vec<String>,
}

/// Which of `paths` name a file under the session's folders: one entry
/// each, in order, `{"path","size"}` or `null`.
pub(super) async fn check_files(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(request): ApiJson<CheckRequest>,
) -> ApiResult<Response> {
    if request.paths.len() > MAX_CHECK_PATHS {
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            &format!("at most {MAX_CHECK_PATHS} paths per check"),
        ));
    }
    let hub = Arc::clone(&app.hub);
    let files = blocking(move || {
        let scope = scope(&hub, &id)?;
        Ok(request
            .paths
            .iter()
            .map(|p| scope.find(p).ok())
            .collect::<Vec<_>>())
    })
    .await?;
    Ok(Json(json!({ "files": files })).into_response())
}
