//! Autopilot from the hub (see `crate::autopilot`):
//!
//! * `GET /api/flywheel/autopilot?folder=P` -- `{"folder", "view",
//!   "sessions"}`: the folder's [`AutopilotView`] and the hub sessions
//!   running its daemon;
//! * `POST /api/flywheel/autopilot/start {"folder", "confirm": true,
//!   "workspace_id"?, "cols"?, "rows"?}` -- `fw autopilot` in a new hub
//!   shell session (201, the session); 409 while one runs there (this
//!   hub's session, or a daemon started elsewhere: its record is live);
//!   400 without `confirm` (it writes the base branch unattended);
//! * `POST .../stop {"folder"}` -- Ctrl-C to this hub's autopilot sessions
//!   there (202); 409 when only a daemon started elsewhere runs;
//! * `POST .../pause|resume {"folder"}` -- the pause marker, as `fw
//!   autopilot --pause|--resume` (`{"paused", "changed"}`);
//! * `POST .../decide {"folder", "run", "decision": "approve"|"deny",
//!   "confirm": true, "gate"?, "attempt"?, "feedback"?}` -- `fw approve` /
//!   `fw reject` bound to the gate shown (200, the enqueued command, and an
//!   `autopilot.gate_decided` event); 409 when that gate is not the one
//!   parked or already has a decision queued, 404 for an unknown run.
//!
//! Actions are refused to peers (`peers::peer_forbidden`): read-only.

use super::flywheel_api::{Refusal, adopted_folder, monitor, off_runtime, still_reading};
use super::{ApiJson, ApiQuery, AppState, error_response};
use crate::autopilot::{
    AutopilotView, GateDecision, GateRefused, MAX_FEEDBACK_CHARS, autopilot_session_command,
    autopilot_session_name, gate_decided_payload,
};
use crate::flywheel::{FlywheelMonitor, runs_autopilot, valid_run_id};
use crate::hub::CreateSession;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Deserialize)]
pub(super) struct FolderQuery {
    folder: String,
}

/// `folder`'s autopilot view: the monitor's latest read, else a fresh one.
async fn current_view(
    monitor: &Arc<FlywheelMonitor>,
    path: &Path,
) -> Result<AutopilotView, Response> {
    if let Some(panel) = monitor.panel(path) {
        return Ok(panel.intake.clone().unwrap_or_else(AutopilotView::absent));
    }
    let Some(guard) = monitor.begin_read(format!("panel\0{}", path.display())) else {
        return Err(still_reading("panel read"));
    };
    let provider = Arc::clone(monitor.provider());
    let folder = path.to_path_buf();
    let panel = off_runtime(READ_TIMEOUT, "reading the panel", move || {
        let panel = provider.panel(&folder);
        drop(guard);
        panel
    })
    .await
    .map_err(IntoResponse::into_response)?;
    Ok(panel.intake.unwrap_or_else(AutopilotView::absent))
}

/// This hub's sessions running `fw autopilot` in `folder` (read from the
/// processes, so off the runtime).
async fn pilot_sessions(app: &AppState, folder: &Path) -> Result<Vec<(String, String)>, Refusal> {
    let hub = Arc::clone(&app.hub);
    let folder = folder.to_path_buf();
    off_runtime(Duration::from_secs(5), "reading the sessions", move || {
        hub.sessions(None)
            .into_iter()
            .filter(|s| runs_autopilot(s, &folder))
            .map(|s| (s.record.id, s.record.name))
            .collect()
    })
    .await
}

fn session_list(sessions: &[(String, String)]) -> serde_json::Value {
    json!(
        sessions
            .iter()
            .map(|(id, name)| json!({ "id": id, "name": name }))
            .collect::<Vec<_>>()
    )
}

/// Resolve `folder` (an adopted folder of a workspace) or answer why not.
async fn resolve(
    app: &AppState,
    folder: String,
) -> Result<(Arc<FlywheelMonitor>, PathBuf, Vec<crate::state::Workspace>), Response> {
    let monitor = monitor(app).map_err(IntoResponse::into_response)?;
    let (path, workspaces) = adopted_folder(app, &monitor, folder)
        .await
        .map_err(IntoResponse::into_response)?;
    Ok((monitor, path, workspaces))
}

pub(super) async fn view(
    State(app): State<AppState>,
    ApiQuery(query): ApiQuery<FolderQuery>,
) -> Response {
    let (monitor, path, _) = match resolve(&app, query.folder).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    let view = match current_view(&monitor, &path).await {
        Ok(view) => view,
        Err(response) => return response,
    };
    let sessions = match pilot_sessions(&app, &path).await {
        Ok(sessions) => sessions,
        Err(refusal) => return refusal.into_response(),
    };
    Json(json!({ "folder": path, "view": view, "sessions": session_list(&sessions) }))
        .into_response()
}

#[derive(Deserialize)]
pub(super) struct StartRequest {
    folder: String,
    workspace_id: Option<String>,
    /// The operator confirmed: autopilot writes the base branch unattended.
    #[serde(default)]
    confirm: bool,
    cols: Option<u16>,
    rows: Option<u16>,
}

/// Why a start did not create a session.
enum StartRefused {
    /// This hub's session runs it.
    Running {
        id: String,
        name: String,
    },
    /// A daemon this hub did not start runs (its record is live).
    Elsewhere {
        pid: Option<i64>,
    },
    Hub(crate::hub::HubError),
}

pub(super) async fn start(
    State(app): State<AppState>,
    ApiJson(request): ApiJson<StartRequest>,
) -> Response {
    if !request.confirm {
        return error_response(
            StatusCode::BAD_REQUEST,
            "autopilot authors tasks and its work lands on the base branch unattended: \
             confirm to start it (\"confirm\": true)",
        );
    }
    let (monitor, path, workspaces) = match resolve(&app, request.folder).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    let workspace = match &request.workspace_id {
        Some(id) => match workspaces.iter().find(|w| &w.id == id) {
            Some(w) => w.clone(),
            None => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    &format!("{} is not a folder of workspace {id}", path.display()),
                );
            }
        },
        None => workspaces[0].clone(),
    };
    // A daemon started elsewhere (the console, a shell) is adopted, never
    // duplicated -- as the console supervisor does with a live record.
    let view = match current_view(&monitor, &path).await {
        Ok(view) => view,
        Err(response) => return response,
    };
    let hub = Arc::clone(&app.hub);
    let starter = Arc::clone(&monitor);
    let folder = path.clone();
    // An action, not a read: no timeout (see `flywheel_api::start_worker`).
    let created = tokio::task::spawn_blocking(move || {
        let _starting = starter.starting();
        let sessions = hub.sessions(None);
        if let Some(running) = sessions.iter().find(|s| runs_autopilot(s, &folder)) {
            return Err(StartRefused::Running {
                id: running.record.id.clone(),
                name: running.record.name.clone(),
            });
        }
        if view.live {
            return Err(StartRefused::Elsewhere { pid: view.pid });
        }
        let name = autopilot_session_name(sessions.iter().map(|s| s.record.name.as_str()));
        let command = autopilot_session_command(&starter.provider().autopilot_command(&folder));
        hub.create_session(CreateSession {
            workspace_id: workspace.id,
            repo: Some(folder),
            name: Some(name),
            kind: Default::default(),
            command: Some(command),
            cols: request.cols,
            rows: request.rows,
            worktree: None,
            ..CreateSession::default()
        })
        .map_err(StartRefused::Hub)
    });
    match created.await {
        Ok(Ok(session)) => {
            monitor.refresh(&path);
            (StatusCode::CREATED, Json(session)).into_response()
        }
        Ok(Err(StartRefused::Running { id, name })) => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!(
                    "autopilot already runs in {} (session {name:?})",
                    path.display()
                ),
                "running": { "id": id, "name": name },
            })),
        )
            .into_response(),
        Ok(Err(StartRefused::Elsewhere { pid })) => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!(
                    "autopilot already runs in {} (pid {}), not started by this hub: it is \
                     adopted, never duplicated; stop it where it was started (the console's \
                     /autopilot stop, or SIGTERM)",
                    path.display(),
                    pid.map_or_else(|| "?".to_owned(), |p| p.to_string())
                ),
                "pid": pid,
            })),
        )
            .into_response(),
        Ok(Err(StartRefused::Hub(e))) => super::ApiError(e).into_response(),
        Err(join) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("starting autopilot failed: {join}"),
        ),
    }
}

#[derive(Deserialize)]
pub(super) struct FolderRequest {
    folder: String,
}

pub(super) async fn stop(
    State(app): State<AppState>,
    ApiJson(request): ApiJson<FolderRequest>,
) -> Response {
    let (monitor, path, _) = match resolve(&app, request.folder).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    let sessions = match pilot_sessions(&app, &path).await {
        Ok(sessions) => sessions,
        Err(refusal) => return refusal.into_response(),
    };
    if sessions.is_empty() {
        let view = match current_view(&monitor, &path).await {
            Ok(view) => view,
            Err(response) => return response,
        };
        let message = if view.live {
            format!(
                "autopilot in {} (pid {}) was not started by this hub: stop it where it was \
                 started (the console's /autopilot stop, or SIGTERM)",
                path.display(),
                view.pid.map_or_else(|| "?".to_owned(), |p| p.to_string())
            )
        } else {
            format!("autopilot is not running in {}", path.display())
        };
        return error_response(StatusCode::CONFLICT, &message);
    }
    // Ctrl-C, as at the terminal: the daemon's SIGINT handler stops it
    // gracefully (its agent sessions' process groups are swept) and the
    // session's wrapper survives it and becomes a shell.
    for (id, _) in &sessions {
        if let Some(live) = app.hub.live_session(id) {
            live.write(vec![0x03]);
        }
    }
    monitor.refresh(&path);
    (
        StatusCode::ACCEPTED,
        Json(json!({ "stopping": session_list(&sessions) })),
    )
        .into_response()
}

async fn set_paused(app: AppState, folder: String, paused: bool) -> Response {
    let (monitor, path, _) = match resolve(&app, folder).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    let provider = Arc::clone(monitor.provider());
    let folder = path.clone();
    let changed = off_runtime(
        Duration::from_secs(10),
        "writing the pause marker",
        move || provider.set_autopilot_paused(&folder, paused),
    )
    .await;
    match changed {
        Ok(Ok(changed)) => {
            monitor.refresh(&path);
            Json(json!({ "paused": paused, "changed": changed })).into_response()
        }
        Ok(Err(message)) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &message),
        Err(refusal) => refusal.into_response(),
    }
}

pub(super) async fn pause(
    State(app): State<AppState>,
    ApiJson(request): ApiJson<FolderRequest>,
) -> Response {
    set_paused(app, request.folder, true).await
}

pub(super) async fn resume(
    State(app): State<AppState>,
    ApiJson(request): ApiJson<FolderRequest>,
) -> Response {
    set_paused(app, request.folder, false).await
}

#[derive(Deserialize)]
pub(super) struct DecideRequest {
    folder: String,
    run: String,
    /// `approve` or `deny`.
    decision: String,
    #[serde(default)]
    confirm: bool,
    gate: Option<u32>,
    attempt: Option<u32>,
    feedback: Option<String>,
}

pub(super) async fn decide(
    State(app): State<AppState>,
    ApiJson(request): ApiJson<DecideRequest>,
) -> Response {
    let approve = match request.decision.as_str() {
        "approve" => true,
        "deny" | "reject" => false,
        other => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!("decision must be approve or deny, not {other:?}"),
            );
        }
    };
    if !request.confirm {
        return error_response(
            StatusCode::BAD_REQUEST,
            "a gate decision is the human's: confirm it (\"confirm\": true)",
        );
    }
    if !valid_run_id(&request.run) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "run must be 1-128 letters, digits, '-' or '_'",
        );
    }
    let feedback = request
        .feedback
        .map(|f| f.trim().to_owned())
        .filter(|f| !f.is_empty());
    if approve && feedback.is_some() {
        return error_response(StatusCode::BAD_REQUEST, "feedback goes with a deny");
    }
    if feedback
        .as_ref()
        .is_some_and(|f| f.chars().count() > MAX_FEEDBACK_CHARS)
    {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!("feedback is limited to {MAX_FEEDBACK_CHARS} characters"),
        );
    }
    let (monitor, path, _) = match resolve(&app, request.folder).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    let with_feedback = feedback.is_some();
    let decision = GateDecision {
        run_id: request.run,
        approve,
        feedback,
        gate: request.gate,
        attempt: request.attempt,
    };
    let provider = Arc::clone(monitor.provider());
    let folder = path.clone();
    // An action: no timeout (a decision enqueued after a give-up would
    // read as failed).
    let decided =
        tokio::task::spawn_blocking(move || provider.decide_gate(&folder, &decision)).await;
    match decided {
        Ok(Ok(done)) => {
            // The hub's own audit trail of the human's decision (the store
            // has the command; this is the timeline's and plugins' copy).
            app.hub.emit_lifecycle(
                "autopilot.gate_decided",
                gate_decided_payload(&path, &done, with_feedback),
            );
            monitor.refresh(&path);
            Json(done).into_response()
        }
        Ok(Err(GateRefused::UnknownRun(m))) => error_response(StatusCode::NOT_FOUND, &m),
        Ok(Err(GateRefused::NotThatGate(m) | GateRefused::AlreadyDecided(m))) => {
            error_response(StatusCode::CONFLICT, &m)
        }
        Ok(Err(GateRefused::Store(m))) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &m),
        Err(join) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("deciding the gate failed: {join}"),
        ),
    }
}
