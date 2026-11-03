//! `/api/plugins`: the plugins on disk, enabling them, their run logs, and
//! running their palette actions. Nothing here installs a plugin: they
//! come only from `<state-dir>/plugins/` on the hub's machine.

use super::ApiQuery;
use super::{ApiJson, AppState, error_response};
use crate::hub::HubEvent;
use crate::plugins::{self, ActionError};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

fn off() -> Response {
    error_response(StatusCode::CONFLICT, "plugins are not running in this hub")
}

pub(super) async fn list(State(app): State<AppState>) -> Response {
    match app.options.plugins.clone() {
        Some(host) => {
            let view = tokio::task::spawn_blocking(move || host.view()).await;
            match view {
                Ok(view) => Json(view).into_response(),
                Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
            }
        }
        None => Json(json!({"enabled": false, "plugins": [], "events": plugins::EVENTS}))
            .into_response(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Toggle {
    enabled: bool,
}

/// Enable or disable a plugin (`plugins.json`).
pub(super) async fn toggle(
    State(app): State<AppState>,
    Path(name): Path<String>,
    ApiJson(body): ApiJson<Toggle>,
) -> Response {
    let Some(host) = app.options.plugins.clone() else {
        return off();
    };
    if host.plugin(&name).is_none() {
        return error_response(StatusCode::NOT_FOUND, &format!("no plugin {name:?}"));
    }
    let done = {
        let host = Arc::clone(&host);
        let name = name.clone();
        tokio::task::spawn_blocking(move || host.set_enabled(&name, body.enabled)).await
    };
    match done {
        Ok(Ok(())) => {
            eprintln!(
                "fw hub: plugin {name} {}",
                if body.enabled { "enabled" } else { "disabled" }
            );
            app.hub.emit(HubEvent::Plugins);
            match host.plugin(&name) {
                Some(plugin) => {
                    Json(plugins::plugin_view(app.hub.state_dir(), &plugin)).into_response()
                }
                None => error_response(StatusCode::NOT_FOUND, &format!("no plugin {name:?}")),
            }
        }
        Ok(Err(message)) => error_response(StatusCode::BAD_REQUEST, &message),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize)]
pub(super) struct LogsQuery {
    limit: Option<usize>,
}

/// A plugin's newest runs, newest first.
pub(super) async fn logs(
    State(app): State<AppState>,
    Path(name): Path<String>,
    ApiQuery(query): ApiQuery<LogsQuery>,
) -> Response {
    if !plugins::valid_name(&name) {
        return error_response(StatusCode::BAD_REQUEST, "not a plugin name");
    }
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let mut runs = plugins::read_logs(app.hub.state_dir(), &name, limit);
    runs.reverse();
    Json(json!({ "plugin": name, "runs": runs })).into_response()
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(super) struct RunBody {
    session_id: Option<String>,
}

/// Run a plugin action: `{"ok","toast","paste","log"}`; 404 unknown
/// plugin/action/session, 409 disabled or off or missing its session.
pub(super) async fn run_action(
    State(app): State<AppState>,
    Path((name, action)): Path<(String, String)>,
    ApiJson(body): ApiJson<RunBody>,
) -> Response {
    let Some(host) = app.options.plugins.clone() else {
        return off();
    };
    match host
        .run_action(&app.hub, &name, &action, body.session_id)
        .await
    {
        Ok(result) => Json(result).into_response(),
        Err(ActionError::NotFound(message)) => error_response(StatusCode::NOT_FOUND, &message),
        Err(ActionError::Refused(message)) => error_response(StatusCode::CONFLICT, &message),
    }
}
