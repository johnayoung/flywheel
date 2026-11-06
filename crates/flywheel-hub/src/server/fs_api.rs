//! The workspace dialog's folder picker on the hub's machine:
//! `GET /api/fs/dirs` (one directory's subdirectories, for completion and
//! the folder browser) and `GET /api/fs/suggestions` (folders worth
//! adding). See [`crate::fsbrowse`]: directory metadata only.

use super::{ApiQuery, AppState, error_response};
use crate::fsbrowse::{self, BrowseError, MAX_ENTRIES, MAX_SUGGESTIONS, ScanBounds};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

#[derive(Deserialize)]
pub(super) struct DirsQuery {
    /// Absolute or `~/...`; blank or absent: the home directory.
    path: Option<String>,
    /// Only names starting with it (any case).
    prefix: Option<String>,
}

/// `{"path","parent","home","git","entries":[{"name","path","git",
/// "hidden","link"?,"error"?}],"truncated"}`: 400 not absolute / not a
/// directory / not readable, 404 missing.
pub(super) async fn dirs(ApiQuery(query): ApiQuery<DirsQuery>) -> Response {
    let listed = tokio::task::spawn_blocking(move || {
        let home = crate::config::home_dir();
        fsbrowse::list_dirs(
            query.path.as_deref().unwrap_or_default(),
            query.prefix.as_deref(),
            home.as_deref(),
            MAX_ENTRIES,
        )
    })
    .await;
    match listed {
        Ok(Ok(listing)) => Json(listing).into_response(),
        Ok(Err(error)) => {
            let status = match error {
                BrowseError::NotFound(_) => StatusCode::NOT_FOUND,
                _ => StatusCode::BAD_REQUEST,
            };
            error_response(status, &error.to_string())
        }
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("worker task failed: {e}"),
        ),
    }
}

/// `{"suggestions":[{"path","git","worktree","source":"used|found",
/// "workspace"?,"modified"?}],"home","roots","truncated"}`: folders the hub's
/// workspaces and sessions use, then repos a bounded scan of the usual
/// roots found (cached a minute).
pub(super) async fn suggestions(State(app): State<AppState>) -> Response {
    let hub = Arc::clone(&app.hub);
    let cache = Arc::clone(&app.repo_scan);
    let found = tokio::task::spawn_blocking(move || {
        let used = hub.used_folders();
        let home = crate::config::home_dir();
        let scan = match &home {
            Some(home) => cache.get(home, ScanBounds::DEFAULT),
            None => Arc::default(),
        };
        json!({
            "home": home,
            "suggestions": fsbrowse::suggestions(&used, &scan, MAX_SUGGESTIONS),
            "roots": scan.roots,
            "truncated": scan.truncated,
        })
    })
    .await;
    match found {
        Ok(body) => Json(body).into_response(),
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("worker task failed: {e}"),
        ),
    }
}
