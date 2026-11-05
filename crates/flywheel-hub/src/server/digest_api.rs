//! Scheduled digests over HTTP (see [`crate::digest`]): `GET /api/digest`
//! (the schedule, next slots, recent digests), `GET /api/digest/preview`,
//! `POST /api/digest/send`, `GET /api/digests/{id}` (a kept digest), and
//! `PUT /api/digest/config` (Settings > Digest: hub.toml's `[digest]`).
//! A peer may read; sending and configuring are the hub owner's.

use super::{ApiJson, ApiQuery, AppState, error_response};
use crate::activity::parse_time;
use crate::config::WriteError;
use crate::digest::{DigestSection, DigestService, valid_id};
use crate::peers::PeerInfo;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

/// A request refused: its status and why.
struct Refused(StatusCode, String);

impl IntoResponse for Refused {
    fn into_response(self) -> Response {
        error_response(self.0, &self.1)
    }
}

fn service(app: &AppState) -> Result<Arc<DigestService>, Refused> {
    app.options.digest.clone().ok_or_else(|| {
        Refused(
            StatusCode::CONFLICT,
            "digests are not available on this hub".to_owned(),
        )
    })
}

fn owner_only(peer: Option<&axum::Extension<PeerInfo>>, what: &str) -> Result<(), Refused> {
    match peer {
        Some(_) => Err(Refused(
            StatusCode::FORBIDDEN,
            format!("{what} is the hub owner's (a peer may only read digests)"),
        )),
        None => Ok(()),
    }
}

fn since_of(raw: Option<&str>) -> Result<Option<chrono::DateTime<chrono::Utc>>, Refused> {
    raw.filter(|r| !r.trim().is_empty())
        .map(|r| parse_time(r, chrono::Utc::now()))
        .transpose()
        .map_err(|message| Refused(StatusCode::BAD_REQUEST, format!("since: {message}")))
}

pub(super) async fn view(State(app): State<AppState>) -> Response {
    match service(&app) {
        Ok(digest) => {
            let mut view = digest.view();
            view["editable"] = json!(app.options.config.is_some());
            Json(view).into_response()
        }
        Err(refused) => refused.into_response(),
    }
}

#[derive(Deserialize, Default)]
pub(super) struct SinceQuery {
    since: Option<String>,
}

/// The digest since the last one (or `since`), neither sent nor kept.
pub(super) async fn preview(
    State(app): State<AppState>,
    ApiQuery(query): ApiQuery<SinceQuery>,
) -> Response {
    let digest = match service(&app) {
        Ok(digest) => digest,
        Err(refused) => return refused.into_response(),
    };
    let since = match since_of(query.since.as_deref()) {
        Ok(since) => since,
        Err(refused) => return refused.into_response(),
    };
    let hub = Arc::clone(&app.hub);
    match tokio::task::spawn_blocking(move || digest.preview(&hub, since)).await {
        Ok(report) => Json(report).into_response(),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(super) struct SendBody {
    since: Option<String>,
}

/// Send a digest now: 200 with it (its `delivery` says where it went).
pub(super) async fn send(
    State(app): State<AppState>,
    peer: Option<axum::Extension<PeerInfo>>,
    ApiJson(body): ApiJson<SendBody>,
) -> Response {
    if let Err(refused) = owner_only(peer.as_ref(), "sending a digest") {
        return refused.into_response();
    }
    let digest = match service(&app) {
        Ok(digest) => digest,
        Err(refused) => return refused.into_response(),
    };
    let since = match since_of(body.since.as_deref()) {
        Ok(since) => since,
        Err(refused) => return refused.into_response(),
    };
    match digest.send_now(&app.hub, since).await {
        Ok(report) => Json(report).into_response(),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

/// A kept digest (the recent ones: [`crate::digest::KEEP_RECENT`]).
pub(super) async fn get_one(State(app): State<AppState>, Path(id): Path<String>) -> Response {
    let digest = match service(&app) {
        Ok(digest) => digest,
        Err(refused) => return refused.into_response(),
    };
    if !valid_id(&id) {
        return error_response(StatusCode::BAD_REQUEST, "not a digest id");
    }
    match digest.get(&id) {
        Some(report) => Json(report).into_response(),
        None => error_response(
            StatusCode::NOT_FOUND,
            "no such digest (the hub keeps its last 14)",
        ),
    }
}

/// Rewrite hub.toml's `[digest]` (Settings > Digest); reloaded at once.
pub(super) async fn put_config(
    State(app): State<AppState>,
    peer: Option<axum::Extension<PeerInfo>>,
    ApiJson(section): ApiJson<DigestSection>,
) -> Response {
    if let Err(refused) = owner_only(peer.as_ref(), "configuring digests") {
        return refused.into_response();
    }
    let Some(config) = app.options.config.clone() else {
        return error_response(
            StatusCode::CONFLICT,
            "this hub runs without a hub.toml watcher",
        );
    };
    let written = {
        let config = Arc::clone(&config);
        tokio::task::spawn_blocking(move || config.write_digest(&section)).await
    };
    match written {
        Ok(Ok(())) => {
            app.hub
                .emit(crate::hub::HubEvent::Config { rev: config.rev() });
            match service(&app) {
                Ok(digest) => {
                    let mut view = digest.view();
                    view["editable"] = json!(true);
                    Json(view).into_response()
                }
                Err(refused) => refused.into_response(),
            }
        }
        Ok(Err(WriteError::Invalid(errors))) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": errors.first().cloned().unwrap_or_default(), "errors": errors })),
        )
            .into_response(),
        Ok(Err(WriteError::Conflict(message))) => error_response(StatusCode::CONFLICT, &message),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}
