//! Holds and decisions (see [`crate::holds`]):
//!
//! * `GET /api/holds[?kind=K]` -- `{"groups": [HoldGroup], "count"}`: what
//!   the operator is waited on for now, grouped by kind and cause (this
//!   hub's sessions, plus the manual gates parked in its adopted folders);
//! * `POST /api/holds/answer {"holds": [id], "answer", "overrides"?:
//!   {id: answer}}` -- answer each hold (`overrides` per hold; `skip`
//!   leaves one out): `{"batch"?, "results": [{"hold", "ok", "said" |
//!   "error"}]}`; two or more answered share a batch id. Refused to peers;
//! * `GET /api/decisions[?kind&session&since&limit]` -- `{"decisions"}`,
//!   newest first (`since`: Unix ms).

use super::flywheel_api::monitor;
use super::{ApiJson, ApiQuery, AppState, blocking, error_response};
use crate::autopilot::{GateDecision, gate_decided_event};
use crate::holds::{self, DecisionQuery, Hold, HoldKind};
use crate::state::new_id;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

/// The most holds one answer takes.
pub const ANSWER_MAX: usize = 200;

#[derive(Deserialize)]
pub(super) struct HoldsQuery {
    kind: Option<String>,
}

fn kind_of(raw: Option<&str>) -> Result<Option<HoldKind>, &'static str> {
    match raw.map(str::trim).filter(|k| !k.is_empty()) {
        None => Ok(None),
        Some(word) => HoldKind::parse(word)
            .map(Some)
            .ok_or("kind is needs_input, awaiting_approval, permission, or gate"),
    }
}

/// Every hold now: the hub's sessions', then every adopted folder's
/// parked gates.
async fn all_holds(app: &AppState) -> Result<Vec<Hold>, Response> {
    let hub = Arc::clone(&app.hub);
    let mut holds = blocking(move || Ok(hub.session_holds()))
        .await
        .map_err(IntoResponse::into_response)?;
    if let Ok(monitor) = monitor(app) {
        for (folder, panel) in monitor.panels() {
            if let Some(intake) = &panel.intake {
                holds.extend(
                    intake
                        .gates
                        .iter()
                        .filter_map(|g| holds::gate_hold(&folder, g)),
                );
            }
        }
    }
    Ok(holds)
}

pub(super) async fn list(
    State(app): State<AppState>,
    ApiQuery(query): ApiQuery<HoldsQuery>,
) -> Response {
    let kind = match kind_of(query.kind.as_deref()) {
        Ok(kind) => kind,
        Err(why) => return error_response(StatusCode::BAD_REQUEST, why),
    };
    let holds = match all_holds(&app).await {
        Ok(holds) => holds,
        Err(response) => return response,
    };
    let holds: Vec<Hold> = holds
        .into_iter()
        .filter(|h| kind.is_none_or(|k| h.kind == k))
        .collect();
    let count = holds.len();
    Json(json!({ "groups": holds::group(holds), "count": count })).into_response()
}

#[derive(Deserialize)]
pub(super) struct AnswerRequest {
    holds: Vec<String>,
    #[serde(default)]
    answer: Option<String>,
    #[serde(default)]
    overrides: HashMap<String, String>,
}

pub(super) async fn answer(
    State(app): State<AppState>,
    ApiJson(request): ApiJson<AnswerRequest>,
) -> Response {
    let mut ids: Vec<String> = Vec::new();
    for id in request.holds {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    if ids.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "name the holds to answer");
    }
    if ids.len() > ANSWER_MAX {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!("one answer takes at most {ANSWER_MAX} holds"),
        );
    }
    let answer_of = |id: &str| {
        request
            .overrides
            .get(id)
            .or(request.answer.as_ref())
            .map(|a| a.trim().to_owned())
            .filter(|a| !a.is_empty())
    };
    if let Some(id) = ids.iter().find(|id| answer_of(id).is_none()) {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!("no answer for {id}: give answer, or one in overrides"),
        );
    }
    let answered = ids
        .iter()
        .filter(|id| answer_of(id).as_deref() != Some(holds::SKIP))
        .count();
    let batch = (answered > 1).then(new_id);
    // Gates are found once, from the monitor's latest read.
    let gates: Vec<Hold> = match monitor(&app) {
        Ok(monitor) => monitor
            .panels()
            .into_iter()
            .flat_map(|(folder, panel)| {
                panel
                    .intake
                    .as_ref()
                    .map(|i| {
                        i.gates
                            .iter()
                            .filter_map(|g| holds::gate_hold(&folder, g))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    let mut results = Vec::with_capacity(ids.len());
    for id in ids {
        let answer = answer_of(&id).unwrap_or_default();
        if answer == holds::SKIP {
            results.push(json!({ "hold": id, "ok": true, "said": "skipped" }));
            continue;
        }
        let outcome = if id.starts_with("gate:") {
            answer_gate(&app, &gates, &id, &answer, batch.as_deref()).await
        } else {
            let hub = Arc::clone(&app.hub);
            let (hold, batch) = (id.clone(), batch.clone());
            blocking(move || hub.answer_session_hold(&hold, &answer, batch.as_deref()))
                .await
                .map_err(|e| e.0.to_string())
        };
        results.push(match outcome {
            Ok(said) => json!({ "hold": id, "ok": true, "said": said }),
            Err(error) => json!({ "hold": id, "ok": false, "error": error }),
        });
    }
    let mut body = json!({ "results": results });
    if let Some(batch) = batch {
        body["batch"] = Value::String(batch);
    }
    Json(body).into_response()
}

/// Approve or deny the parked gate `id` names, as `POST
/// /api/flywheel/autopilot/decide` does, and record it.
async fn answer_gate(
    app: &AppState,
    gates: &[Hold],
    id: &str,
    answer: &str,
    batch: Option<&str>,
) -> Result<String, String> {
    let hold = gates
        .iter()
        .find(|h| h.id == id)
        .cloned()
        .ok_or_else(|| format!("the hold {id} (decided already, or gone)"))?;
    let (approve, feedback) = holds::gate_answer(answer)?;
    let mut parts = id.splitn(4, ':').skip(1);
    let run = parts.next().unwrap_or_default().to_owned();
    let gate = parts.next().and_then(|g| g.parse().ok());
    let attempt = parts.next().and_then(|a| a.parse().ok());
    let folder = hold.folder.clone().ok_or("the gate names no folder")?;
    let monitor = monitor(app).map_err(|_| "the Flywheel panel is not available".to_owned())?;
    let provider = Arc::clone(monitor.provider());
    let decision = GateDecision {
        run_id: run,
        approve,
        feedback: feedback.clone(),
        gate,
        attempt,
    };
    let at = folder.clone();
    let decided = tokio::task::spawn_blocking(move || provider.decide_gate(&at, &decision))
        .await
        .map_err(|join| format!("deciding the gate failed: {join}"))?
        .map_err(|refused| format!("{refused:?}"))?;
    let (event, payload) = gate_decided_event(&folder, &decided, feedback.is_some());
    app.hub.emit_lifecycle(event, payload);
    monitor.refresh(&folder);
    let mut new = holds::gate_decision(Some(hold), &decided, feedback.as_deref());
    new.batch = batch.map(str::to_owned);
    app.hub.record_decision(new);
    Ok(if approve { "approved" } else { "denied" }.to_owned())
}

#[derive(Deserialize)]
pub(super) struct DecisionsQuery {
    kind: Option<String>,
    session: Option<String>,
    since: Option<i64>,
    limit: Option<usize>,
}

pub(super) async fn decisions(
    State(app): State<AppState>,
    ApiQuery(query): ApiQuery<DecisionsQuery>,
) -> Response {
    let kind = match kind_of(query.kind.as_deref()) {
        Ok(kind) => kind,
        Err(why) => return error_response(StatusCode::BAD_REQUEST, why),
    };
    let query = DecisionQuery {
        kind,
        session: query.session.filter(|s| !s.trim().is_empty()),
        since: query.since,
        limit: query.limit,
    };
    let hub = Arc::clone(&app.hub);
    match tokio::task::spawn_blocking(move || hub.decisions().query(&query)).await {
        Ok(Ok(decisions)) => Json(json!({ "decisions": decisions })).into_response(),
        Ok(Err(e)) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &e),
        Err(join) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &join.to_string()),
    }
}
