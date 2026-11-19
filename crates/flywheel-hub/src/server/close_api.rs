//! Closing a session's directory (see [`crate::close`]): `GET
//! /api/sessions/{id}/checklist` (its close checklist now) and `POST
//! /api/sessions/{id}/close` `{"worktree"?,"build"?,"report"?,"dry_run"?}`
//! (the operator ends it at rest, then applies the choices; 409 while it
//! works), and `GET /api/sessions/{id}/report` (its kept final report).

use super::{ApiJson, ApiResult, AppState, blocking};
use crate::close::CloseChoices;
use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use std::sync::Arc;

/// Its final report (the hub's copy), as text: 404 when none was kept.
pub(super) async fn report(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let text = blocking(move || {
        let id = hub.session_id_named(&id)?;
        let stored = hub
            .session(&id)?
            .record
            .ended
            .and_then(|e| e.close)
            .and_then(|c| c.report)
            .map(|r| r.stored)
            .ok_or_else(|| crate::state::StateError::NotFound(format!("a report of {id}")))?;
        std::fs::read_to_string(&stored)
            .map_err(|e| crate::hub::HubError::Internal(format!("{}: {e}", stored.display())))
    })
    .await?;
    Ok((
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        text,
    )
        .into_response())
}

pub(super) async fn checklist(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let list = blocking(move || {
        let id = hub.session_id_named(&id)?;
        hub.close_checklist(&id)
    })
    .await?;
    let text = list.text();
    Ok(Json(serde_json::json!({ "checklist": list, "text": text })).into_response())
}

#[derive(Deserialize)]
pub(super) struct CloseBody {
    #[serde(flatten)]
    choices: CloseChoices,
    #[serde(default)]
    dry_run: bool,
}

pub(super) async fn close(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<CloseBody>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let done = blocking(move || {
        let id = hub.session_id_named(&id)?;
        hub.operator_close(&id, body.choices, body.dry_run)
    })
    .await?;
    Ok(Json(done).into_response())
}
