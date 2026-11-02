//! VS Code workspace import: `POST /api/workspaces/import` and the
//! picker's `GET /api/code-workspaces` listing of `*.code-workspace` files
//! on the hub's machine.

use super::{ApiJson, ApiResult, AppState, blocking, error_response};
use crate::state::expand_home;
use crate::vscode::{self, DEFAULT_SEARCH_DEPTH, ResolveEnv};
use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Deserialize)]
pub(super) struct ImportRequest {
    /// The workspace file on the hub's machine (absolute or `~/...`).
    path: PathBuf,
    name: Option<String>,
    #[serde(default)]
    update: bool,
}

/// Import a workspace file: 201 with the report when it created the
/// workspace, 200 when `update` replaced an existing one's folders.
pub(super) async fn import_workspace(
    State(app): State<AppState>,
    ApiJson(request): ApiJson<ImportRequest>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let report = blocking(move || {
        hub.import_code_workspace(&request.path, request.name.as_deref(), request.update)
    })
    .await?;
    let status = if report.created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(report)).into_response())
}

#[derive(Deserialize)]
pub(super) struct FindQuery {
    /// Search root (absolute or `~/...`); default the hub's import root.
    root: Option<String>,
}

/// `{"root","depth","files":[{"path","name","folders","skipped",
/// "workspace_id"}|{"path","name","error"}],"truncated"}`: the workspace
/// files under `root`, each with its folder count and the id of the
/// workspace already named like it (an import would need `update`).
pub(super) async fn find_code_workspaces(
    State(app): State<AppState>,
    Query(query): Query<FindQuery>,
) -> Response {
    let env = ResolveEnv::current();
    let root = match query
        .root
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        Some(raw) => expand_home(&PathBuf::from(raw), env.home.as_deref()),
        None => match app.hub.import_root() {
            Some(root) => root.to_path_buf(),
            None => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "no search root: pass root (HOME is not set)",
                );
            }
        },
    };
    if !root.is_absolute() {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!("search root {} must be absolute", root.display()),
        );
    }
    if !root.is_dir() {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!("search root {} is not a directory", root.display()),
        );
    }
    let hub = Arc::clone(&app.hub);
    let listed = tokio::task::spawn_blocking(move || {
        let found = vscode::discover(&root, DEFAULT_SEARCH_DEPTH);
        let workspaces = hub.workspaces();
        let files: Vec<Value> = found
            .files
            .iter()
            .map(|file| match vscode::read(file, &env) {
                Ok(parsed) => json!({
                    "path": file,
                    "name": parsed.name,
                    "folders": parsed.folders.len(),
                    "skipped": parsed.skipped.len(),
                    "workspace_id": workspaces
                        .iter()
                        .find(|w| w.name == parsed.name)
                        .map(|w| w.id.clone()),
                }),
                Err(error) => json!({
                    "path": file,
                    "name": vscode::default_name(file),
                    "error": error,
                }),
            })
            .collect();
        json!({
            "root": root,
            "depth": DEFAULT_SEARCH_DEPTH,
            "files": files,
            "truncated": found.truncated,
        })
    })
    .await;
    match listed {
        Ok(body) => Json(body).into_response(),
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("worker task failed: {e}"),
        ),
    }
}
