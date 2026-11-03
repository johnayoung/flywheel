//! Sessions: `/api/sessions` (list, get, create, rename, kill, restart),
//! the agent kinds, folder checks, a session's screen text, and typed input.

use super::ApiQuery;
use super::{ApiError, ApiJson, ApiResult, AppState, MAX_SCREEN_LINES, blocking, error_response};
use crate::hub::{CreateSession, HubError};
use crate::pty::WriteOutcome;
use crate::state::{is_git_root, validate_folder};
use crate::vt::{DEFAULT_RECENT_LINES, TextSource};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

#[derive(Deserialize)]
pub(super) struct SessionFilter {
    workspace_id: Option<String>,
}

pub(super) async fn list_sessions(
    State(app): State<AppState>,
    ApiQuery(filter): ApiQuery<SessionFilter>,
) -> Response {
    Json(json!({ "sessions": app.hub.sessions(filter.workspace_id.as_deref()) })).into_response()
}

pub(super) async fn get_session(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    Ok(Json(app.hub.session(&id)?).into_response())
}

pub(super) async fn create_session(
    State(app): State<AppState>,
    ApiJson(request): ApiJson<CreateSession>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let view = blocking(move || hub.create_session(request)).await?;
    Ok((StatusCode::CREATED, Json(view)).into_response())
}

#[derive(Deserialize)]
pub(super) struct SessionPatch {
    name: Option<String>,
}

pub(super) async fn update_session(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(patch): ApiJson<SessionPatch>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let view = blocking(move || match patch.name {
        Some(name) => hub.rename_session(&id, &name),
        None => Ok(hub.session(&id)?),
    })
    .await?;
    Ok(Json(view).into_response())
}

pub(super) async fn list_kinds(State(app): State<AppState>) -> Response {
    let hub = Arc::clone(&app.hub);
    match tokio::task::spawn_blocking(move || hub.kinds()).await {
        Ok(kinds) => Json(json!({ "kinds": kinds })).into_response(),
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("worker task failed: {e}"),
        ),
    }
}

#[derive(Deserialize)]
pub(super) struct RepoQuery {
    path: Option<String>,
}

/// Validate one candidate folder path for the workspace form: always 200,
/// `{"ok":true,"path":canonical,"git":bool}` or `{"ok":false,"error":...}`
/// (a missing or blank `path` included).
pub(super) async fn check_folder(ApiQuery(query): ApiQuery<RepoQuery>) -> Response {
    let Some(path) = query.path.filter(|p| !p.trim().is_empty()) else {
        return Json(json!({ "ok": false, "error": "path is required" })).into_response();
    };
    let raw = std::path::PathBuf::from(path.trim());
    let result = tokio::task::spawn_blocking(move || {
        validate_folder(&raw).map(|path| {
            let git = is_git_root(&path);
            (path, git)
        })
    })
    .await;
    match result {
        Ok(Ok((path, git))) => {
            Json(json!({ "ok": true, "path": path, "git": git })).into_response()
        }
        Ok(Err(e)) => Json(json!({ "ok": false, "error": e.to_string() })).into_response(),
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("worker task failed: {e}"),
        ),
    }
}

pub(super) async fn kill_session(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    blocking(move || hub.kill_session(&id)).await?;
    Ok(StatusCode::ACCEPTED.into_response())
}

#[derive(Deserialize, Default)]
pub(super) struct RestartRequest {
    cols: Option<u16>,
    rows: Option<u16>,
}

pub(super) async fn restart_session(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(request): ApiJson<RestartRequest>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let view = blocking(move || hub.restart_session(&id, request.cols, request.rows)).await?;
    Ok(Json(view).into_response())
}

#[derive(Deserialize)]
pub(super) struct ScreenQuery {
    source: Option<String>,
    /// Parsed by hand so a bad value gets the API's JSON error.
    lines: Option<String>,
}

/// The rendered text of a session's terminal: `source=visible` (default)
/// is the screen's rows; `source=recent` the last `lines` logical lines of
/// scrollback plus screen.
pub(super) async fn session_screen(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiQuery(query): ApiQuery<ScreenQuery>,
) -> ApiResult<Response> {
    // An unknown session is a 404 whatever the options say.
    let label = app.hub.session(&id)?.record.label();
    let lines = match query.lines.as_deref() {
        None => DEFAULT_RECENT_LINES,
        Some(raw) => match raw.parse::<usize>() {
            Ok(n) if (1..=MAX_SCREEN_LINES).contains(&n) => n,
            _ => {
                return Ok(error_response(
                    StatusCode::BAD_REQUEST,
                    &format!("lines must be an integer between 1 and {MAX_SCREEN_LINES}"),
                ));
            }
        },
    };
    let source = match query.source.as_deref().unwrap_or("visible") {
        "visible" if query.lines.is_some() => {
            return Ok(error_response(
                StatusCode::BAD_REQUEST,
                "lines applies to source=recent only (visible is always the whole screen)",
            ));
        }
        "visible" => TextSource::Visible,
        "recent" => TextSource::Recent { lines },
        other => {
            return Ok(error_response(
                StatusCode::BAD_REQUEST,
                &format!("unknown source {other:?} (visible or recent)"),
            ));
        }
    };
    let Some(session) = app.hub.live_session(&id) else {
        return Ok(error_response(
            StatusCode::NOT_FOUND,
            &format!("session {label} has no terminal on this server"),
        ));
    };
    let text = blocking(move || Ok(session.screen_text(source))).await?;
    let source = match source {
        TextSource::Visible => "visible",
        TextSource::Recent { .. } => "recent",
    };
    let mut body =
        serde_json::to_value(text).map_err(|e| ApiError(HubError::Internal(e.to_string())))?;
    body["source"] = json!(source);
    Ok(Json(body).into_response())
}

#[derive(Deserialize)]
pub(super) struct InputRequest {
    #[serde(default)]
    data: String,
    /// Send `data` the way a terminal pastes it: newlines as `CR`, wrapped
    /// in bracketed-paste markers when the program enabled them.
    #[serde(default)]
    paste: bool,
    /// Press Enter afterwards (a separate `CR`, [`ENTER_DELAY`](crate::pty::ENTER_DELAY) later).
    #[serde(default)]
    enter: bool,
}

fn input_refused(outcome: WriteOutcome, label: &str) -> Option<Response> {
    match outcome {
        WriteOutcome::Queued => None,
        WriteOutcome::Closed => Some(error_response(
            StatusCode::CONFLICT,
            &format!("session {label} has exited"),
        )),
        WriteOutcome::Backlogged => Some(error_response(
            StatusCode::CONFLICT,
            "input dropped: the terminal is not reading its input",
        )),
        WriteOutcome::HandingOff => Some(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "input dropped: the hub is handing over to a new process; retry",
        )),
    }
}

/// Type into a session (for scripts and `fw hub send`): `data` as is, or
/// pasted (`paste`), then optionally Enter. Answers
/// `{"bytes","bracketed"}`.
pub(super) async fn session_input(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(request): ApiJson<InputRequest>,
) -> ApiResult<Response> {
    let view = app.hub.session(&id)?;
    let label = view.record.label();
    let Some(session) = app.hub.live_session(&id) else {
        return Ok(error_response(
            StatusCode::CONFLICT,
            &format!("session {label} has no terminal on this server"),
        ));
    };
    if session.exit_info().is_some() {
        return Ok(error_response(
            StatusCode::CONFLICT,
            &format!("session {label} has exited"),
        ));
    }
    if request.data.is_empty() && !request.enter {
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            "nothing to send: give data and/or enter",
        ));
    }
    let bracketed = request.paste && session.bracketed_paste();
    let bytes = if request.paste {
        crate::pty::paste_bytes(&request.data, bracketed)
    } else {
        request.data.into_bytes()
    };
    let mut sent = bytes.len();
    if !bytes.is_empty()
        && let Some(refused) = input_refused(session.write(bytes), &label)
    {
        return Ok(refused);
    }
    if request.enter {
        if sent > 0 {
            tokio::time::sleep(crate::pty::ENTER_DELAY).await;
        }
        if let Some(refused) = input_refused(session.write(b"\r".to_vec()), &label) {
            return Ok(refused);
        }
        sent += 1;
        // A command may have run: its git badge is looked at shortly.
        app.hub.nudge_badge_soon(&view.record);
    }
    Ok(Json(json!({ "bytes": sent, "bracketed": bracketed })).into_response())
}
