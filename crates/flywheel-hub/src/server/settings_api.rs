//! `GET/PUT /api/settings` (the UI's preferences and keymap, kept by the
//! hub), `POST /api/settings/keys/check` (a keymap's conflicts, for a
//! capture or a device-only keymap), and `GET /api/config` (the
//! effective `hub.toml`, read-only).

use super::{ApiJson, AppState, error_response};
use crate::hub::HubEvent;
use crate::settings::{Patch, SaveError, keys};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;

/// The settings view plus this hub's id (a browser keys its per-device
/// overrides by it: another hub later on the same origin starts clean).
fn settings_view(app: &AppState) -> serde_json::Value {
    let mut view = app.settings.view();
    view["hub_id"] = json!(app.machines.hub_id());
    view
}

pub(super) async fn get_settings(State(app): State<AppState>) -> Response {
    Json(settings_view(&app)).into_response()
}

/// Save a patch written against `rev`: 200 the settings (+ warnings),
/// 409 stale (with the current settings), 422 invalid (every finding).
pub(super) async fn put_settings(
    State(app): State<AppState>,
    ApiJson(patch): ApiJson<Patch>,
) -> Response {
    let store = Arc::clone(&app.settings);
    let saved = tokio::task::spawn_blocking(move || store.save(patch)).await;
    match saved {
        Ok(Ok(saved)) => {
            app.hub.emit(HubEvent::Settings {
                rev: saved.settings.rev,
            });
            let mut view = settings_view(&app);
            view["warnings"] = json!(saved.warnings);
            Json(view).into_response()
        }
        Ok(Err(SaveError::Stale(_))) => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "settings changed elsewhere; re-read and apply again",
                "settings": settings_view(&app),
            })),
        )
            .into_response(),
        Ok(Err(SaveError::Invalid(errors))) => {
            let first = errors
                .first()
                .map(|f| f.message.clone())
                .unwrap_or_default();
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "error": first, "errors": errors })),
            )
                .into_response()
        }
        Ok(Err(SaveError::Io(message))) => {
            error_response(StatusCode::INTERNAL_SERVER_ERROR, &message)
        }
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct KeysCheck {
    /// Action -> bindings, for every action not on its default.
    keys: BTreeMap<String, Vec<String>>,
}

/// Check a keymap without saving it: `{"keys","errors","warnings"}`.
pub(super) async fn check_keys(ApiJson(body): ApiJson<KeysCheck>) -> Response {
    Json(keys::check(&body.keys)).into_response()
}

/// The effective `hub.toml` (with each value's source), the file's state,
/// and edits waiting for a restart.
pub(super) async fn get_config(State(app): State<AppState>) -> Response {
    match &app.options.config {
        Some(watch) => Json(watch.view()).into_response(),
        None => {
            let home = crate::config::home_dir();
            match crate::config::offline_view(app.hub.state_dir(), home.as_deref()) {
                Ok(view) => Json(view).into_response(),
                Err(message) => (
                    StatusCode::OK,
                    Json(json!({
                        "file": {"path": app.hub.state_dir().join(crate::config::CONFIG_FILE).to_string_lossy(), "exists": true, "error": message},
                        "values": [],
                        "pending_restart": [],
                        "running": false,
                    })),
                )
                    .into_response(),
            }
        }
    }
}
