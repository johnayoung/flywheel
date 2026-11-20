//! Leases over HTTP (see [`crate::leases`]): `GET /api/leases` (every
//! lease and wait, `?resource=` one's), `POST /api/leases` (ask; waits in
//! the call up to `wait_secs`, at most `block_secs` (50 s): 200 granted or renewed, 202
//! still in line with its waiter, 409 busy when it would not wait),
//! `GET /api/leases/waiters/{id}?wait=` (collect a wait's grant: 200,
//! 202 still waiting, 404 gone), `DELETE /api/leases/waiters/{id}` (take
//! a wait back), `POST /api/leases/{id}/renew` `{holder?, ttl_secs?}`,
//! and `POST /api/leases/{id}/release` `{holder?, break?}` (break: the
//! operator ends another's).
//!
//! The caller is the operator (the hub token) or, with `holder`, a
//! process it names (`fw hub lease`): its host, pid, start time, and the
//! session it runs in (`FLYWHEEL_HUB_SESSION`), whose end drops it.

use super::{ApiJson, ApiQuery, ApiResult, AppState, blocking};
use crate::hub::leases::Outcome;
use crate::leases::{Ask, DEFAULT_TTL_SECS, Holder, MAX_BLOCK_SECS, Mode};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

#[derive(Deserialize, Default)]
pub(super) struct LeaseFilter {
    resource: Option<String>,
}

/// A process holding leases, as `fw hub lease` names itself.
#[derive(Deserialize, Clone)]
pub(super) struct ProcessHolder {
    host: String,
    pid: u32,
    #[serde(default)]
    started: Option<u64>,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    label: Option<String>,
}

impl ProcessHolder {
    fn holder(&self, hub: &crate::hub::Hub) -> Result<Holder, crate::hub::HubError> {
        // A session of another hub (the process runs inside one, and asks
        // this hub) is not one this hub can wait on: the process alone.
        let session = self
            .session
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .and_then(|s| hub.session_id_named(s).ok());
        Ok(Holder::Process {
            host: self.host.clone(),
            pid: self.pid,
            started: self.started,
            session,
            label: self
                .label
                .clone()
                .filter(|l| !l.trim().is_empty())
                .unwrap_or_else(|| "fw hub lease".to_owned()),
        })
    }
}

#[derive(Deserialize)]
pub(super) struct NewLease {
    resource: String,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    ttl_secs: Option<u32>,
    #[serde(default)]
    wait_secs: Option<u32>,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    holder: Option<ProcessHolder>,
    /// How long to wait inside this call (default and most: 50 s); the
    /// rest of `wait_secs` is waited by collecting the waiter.
    #[serde(default)]
    block_secs: Option<u32>,
}

#[derive(Deserialize, Default)]
pub(super) struct WaitQuery {
    wait: Option<u32>,
}

#[derive(Deserialize, Default)]
pub(super) struct ByHolder {
    #[serde(default)]
    holder: Option<ProcessHolder>,
    #[serde(default)]
    ttl_secs: Option<u32>,
    /// Release: the operator ends another's lease.
    #[serde(default, rename = "break")]
    break_it: bool,
}

fn answer(outcome: Outcome) -> Response {
    let status = match &outcome {
        Outcome::Granted { .. } | Outcome::Renewed { .. } => StatusCode::OK,
        Outcome::Waiting { .. } => StatusCode::ACCEPTED,
        Outcome::Busy { .. } => StatusCode::CONFLICT,
    };
    (status, Json(outcome)).into_response()
}

pub(super) async fn list_leases(
    State(app): State<AppState>,
    ApiQuery(filter): ApiQuery<LeaseFilter>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let view = blocking(move || {
        let resource = match filter.resource.as_deref().filter(|r| !r.trim().is_empty()) {
            Some(r) => Some(
                crate::leases::validate_resource(r).map_err(crate::state::StateError::Invalid)?,
            ),
            None => None,
        };
        Ok(hub.leases(resource.as_deref()))
    })
    .await?;
    Ok(Json(view).into_response())
}

pub(super) async fn acquire_lease(
    State(app): State<AppState>,
    ApiJson(new): ApiJson<NewLease>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let outcome = blocking(move || {
        let holder = match &new.holder {
            Some(process) => process.holder(&hub)?,
            None => Holder::Operator,
        };
        let mode = Mode::parse(new.mode.as_deref().unwrap_or(""))
            .map_err(crate::state::StateError::Invalid)?;
        let wait = new.wait_secs.unwrap_or(0);
        hub.lease_acquire(
            Ask {
                resource: new.resource,
                mode,
                holder,
                ttl_secs: new.ttl_secs.unwrap_or(DEFAULT_TTL_SECS),
                wait_secs: wait,
                note: new.note,
                within: Vec::new(),
            },
            new.block_secs.unwrap_or(MAX_BLOCK_SECS),
        )
    })
    .await?;
    Ok(answer(outcome))
}

pub(super) async fn collect_wait(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiQuery(query): ApiQuery<WaitQuery>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let got = blocking(move || {
        let lease = hub.lease_wait(&id, query.wait.unwrap_or(0).min(MAX_BLOCK_SECS))?;
        Ok((
            lease,
            hub.leases(None).waiters.into_iter().find(|w| w.id == id),
        ))
    })
    .await?;
    Ok(match got {
        (Some(lease), _) => (
            StatusCode::OK,
            Json(json!({ "state": "granted", "lease": lease })),
        )
            .into_response(),
        (None, waiter) => (
            StatusCode::ACCEPTED,
            Json(json!({ "state": "waiting", "waiter": waiter })),
        )
            .into_response(),
    })
}

pub(super) async fn cancel_wait(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let waiter = blocking(move || hub.lease_cancel_wait(None, &id)).await?;
    Ok(Json(waiter).into_response())
}

pub(super) async fn renew_lease(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(by): ApiJson<ByHolder>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let outcome = blocking(move || {
        let lease = hub
            .leases(None)
            .leases
            .into_iter()
            .find(|l| l.id == id || (id.len() >= 4 && l.id.starts_with(id.trim_start_matches('l'))))
            .ok_or_else(|| crate::state::StateError::NotFound(format!("a lease {id:?}")))?;
        let holder = match &by.holder {
            Some(process) => process.holder(&hub)?,
            None => lease.holder.clone(),
        };
        hub.lease_acquire(
            Ask {
                resource: lease.resource,
                mode: lease.mode,
                holder,
                ttl_secs: by.ttl_secs.unwrap_or(lease.ttl_secs),
                wait_secs: 0,
                note: None,
                within: Vec::new(),
            },
            0,
        )
    })
    .await?;
    Ok(answer(outcome))
}

pub(super) async fn release_lease(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(by): ApiJson<ByHolder>,
) -> ApiResult<Response> {
    let hub = Arc::clone(&app.hub);
    let break_it = by.break_it;
    let lease = blocking(move || {
        let holder = match &by.holder {
            Some(process) => Some(process.holder(&hub)?),
            None => None,
        };
        hub.lease_release(holder.as_ref(), &id, break_it)
    })
    .await?;
    Ok(Json(lease).into_response())
}
