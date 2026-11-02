//! `GET /api/notify` (whether push notifications are configured, and to
//! which host) and `POST /api/notify/test` (send a test push).

use super::{AppState, error_response};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// `{"push": null}` when off, else the host (never the URL: its path is
/// the secret), the statuses that push, and the timings.
pub(super) async fn status(State(app): State<AppState>) -> Response {
    let push = app.options.notifier.as_ref().map(|notifier| {
        let config = notifier.config();
        json!({
            "host": config.host(),
            "events": config.events(),
            "debounce_secs": config.debounce.as_secs(),
            "settle_secs": config.settle.as_secs(),
        })
    });
    Json(json!({ "push": push })).into_response()
}

/// 204 once the webhook accepted a test push; 409 when push is off; 502
/// with the failure when the webhook did not take it.
pub(super) async fn test(State(app): State<AppState>) -> Response {
    let Some(notifier) = app.options.notifier.clone() else {
        return error_response(
            StatusCode::CONFLICT,
            "push notifications are off: start the hub with --notify-url or <state-dir>/notify.toml",
        );
    };
    match notifier.send_test().await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => error_response(StatusCode::BAD_GATEWAY, &e),
    }
}
