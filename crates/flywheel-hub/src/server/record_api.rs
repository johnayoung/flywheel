//! Session recordings (see [`crate::record`]): start / stop a session's
//! recording, list, download, and delete `.cast` files, and the replay
//! player page (`/replay#<name>`, a signed-in browser's).

use super::auth::{Auth, authenticate, no_store, too_many_attempts};
use super::{ApiResult, AppState, Client, blocking, error_response};
use crate::web;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde_json::{Value, json};
use std::sync::Arc;

pub(super) async fn start(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let (info, started) = blocking(move || hub.start_recording(&id)).await?;
    let status = if started {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((
        status,
        Json(json!({ "recording": info, "started": started })),
    )
        .into_response())
}

pub(super) async fn stop(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let hub = Arc::clone(&app.hub);
    let info = blocking(move || hub.stop_recording(&id)).await?;
    Ok(Json(json!({ "recording": info })))
}

pub(super) async fn list(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    let hub = Arc::clone(&app.hub);
    let recordings = blocking(move || Ok(hub.recordings().list()?)).await?;
    let limits = app.hub.recordings().limits();
    let total: u64 = recordings.iter().map(|r| r.size).sum();
    Ok(Json(json!({
        "dir": app.hub.recordings().dir(),
        "limits": { "max_file_bytes": limits.max_file_bytes, "max_total_bytes": limits.max_total_bytes },
        "total_bytes": total,
        "recordings": recordings,
    })))
}

/// The `.cast` file itself (asciinema v2; an active one as written so
/// far), always as a download.
pub(super) async fn download(
    State(app): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let lookup = name.clone();
    let path = blocking(move || Ok(hub.recordings().path_of(&lookup)?)).await?;
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(e) => {
            return Ok(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("reading {name}: {e}"),
            ));
        }
    };
    let mut response = bytes.into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-asciicast"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    // `name` is a validated plain file name (no quotes or separators).
    if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    Ok(response)
}

pub(super) async fn delete(
    State(app): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    blocking(move || Ok(hub.recordings().delete(&name)?)).await?;
    app.hub.emit(crate::hub::HubEvent::Sessions);
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// The replay player: a signed-in browser's page (the token or a login
/// cookie); anything else goes to sign-in.
pub(super) async fn player(
    State(app): State<AppState>,
    client: Client,
    headers: HeaderMap,
) -> Response {
    match authenticate(&app, &headers, client) {
        Auth::Granted(_) => no_store(Html(web::PLAYER_HTML).into_response()),
        Auth::Limited(wait) => too_many_attempts(wait),
        Auth::Peer(_) | Auth::View => error_response(
            StatusCode::FORBIDDEN,
            "the replay player takes the hub's token or a browser login",
        ),
        Auth::Anonymous | Auth::Denied => Redirect::to("/").into_response(),
    }
}
