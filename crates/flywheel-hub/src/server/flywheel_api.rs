//! The Flywheel panel's API (see `crate::flywheel`):
//!
//! * `GET /api/flywheel` -- `{"enabled", "folders": [summary...]}`;
//! * `GET /api/flywheel/panel?folder=P` -- one folder's whole panel;
//! * `GET /api/flywheel/run?folder=P&run=R[&since=N]` -- a run's `fw show`
//!   summary and its transcript from entry `N` on;
//! * `POST /api/flywheel/worker {"folder", "workspace_id"?, "cols"?,
//!   "rows"?}` -- start `fw worker` in a new hub shell session there;
//! * `{"type":"flywheel"}` messages on the events socket.
//!
//! Only folders of the hub's workspaces are served, and only adopted ones.

use super::{AppState, error_response};
use crate::flywheel::{
    FlywheelEvent, FlywheelMonitor, FlywheelSummary, valid_run_id, worker_session_command,
};
use crate::hub::{CreateSession, Hub};
use crate::state::Workspace;
use axum::Json;
use axum::extract::ws::Message;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

/// The longest a run-output read may take (a huge transcript on a slow
/// disk, an unreachable Postgres) before the request gives up.
const RUN_OUTPUT_TIMEOUT: Duration = Duration::from_secs(20);
/// The same for a panel read the monitor has not cached yet.
const PANEL_TIMEOUT: Duration = Duration::from_secs(20);

/// A refused request: its status and message (kept small, unlike a
/// built `Response`, so `Result`s carrying it stay cheap).
struct Refusal {
    status: StatusCode,
    message: String,
}

impl Refusal {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        error_response(self.status, &self.message)
    }
}

fn monitor(app: &AppState) -> Result<Arc<FlywheelMonitor>, Refusal> {
    app.options.flywheel.clone().ok_or_else(|| {
        Refusal::new(
            StatusCode::NOT_FOUND,
            "the Flywheel panel is not available in this hub",
        )
    })
}

/// `folder` as one of the workspaces' (canonical) folder paths, with the
/// workspaces that hold it; `Err` answers 404.
fn workspace_folder(hub: &Hub, folder: &str) -> Result<(PathBuf, Vec<Workspace>), Refusal> {
    let raw = PathBuf::from(folder);
    let workspaces = hub.workspaces();
    let holding = |path: &Path| -> Vec<Workspace> {
        workspaces
            .iter()
            .filter(|w| w.has_folder(path))
            .cloned()
            .collect()
    };
    let mut found = holding(&raw);
    let mut path = raw.clone();
    if found.is_empty()
        && let Ok(canonical) = raw.canonicalize()
    {
        found = holding(&canonical);
        path = canonical;
    }
    if found.is_empty() {
        return Err(Refusal::new(
            StatusCode::NOT_FOUND,
            format!("{folder} is not a folder of any workspace"),
        ));
    }
    Ok((path, found))
}

async fn off_runtime<T: Send + 'static>(
    limit: Duration,
    what: &str,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Refusal> {
    match tokio::time::timeout(limit, tokio::task::spawn_blocking(f)).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(join)) => Err(Refusal::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("{what} failed: {join}"),
        )),
        Err(_) => Err(Refusal::new(
            StatusCode::GATEWAY_TIMEOUT,
            format!("{what} took longer than {}s", limit.as_secs()),
        )),
    }
}

/// Resolve and check `folder` (a workspace folder, adopted) off the
/// runtime.
async fn adopted_folder(
    app: &AppState,
    monitor: &Arc<FlywheelMonitor>,
    folder: String,
) -> Result<(PathBuf, Vec<Workspace>), Refusal> {
    let hub = Arc::clone(&app.hub);
    let provider = Arc::clone(monitor.provider());
    off_runtime(Duration::from_secs(5), "checking the folder", move || {
        let (path, workspaces) = workspace_folder(&hub, &folder)?;
        if !provider.adopted(&path) {
            return Err(Refusal::new(
                StatusCode::NOT_FOUND,
                format!(
                    "{} is not a flywheel repo (no flywheel.toml or .flywheel/)",
                    path.display()
                ),
            ));
        }
        Ok((path, workspaces))
    })
    .await?
}

pub(super) async fn list(State(app): State<AppState>) -> Response {
    match &app.options.flywheel {
        Some(monitor) => {
            Json(json!({ "enabled": true, "folders": monitor.summaries() })).into_response()
        }
        None => Json(json!({ "enabled": false, "folders": [] })).into_response(),
    }
}

#[derive(Deserialize)]
pub(super) struct PanelQuery {
    folder: String,
}

pub(super) async fn panel(
    State(app): State<AppState>,
    Query(query): Query<PanelQuery>,
) -> Response {
    let monitor = match monitor(&app) {
        Ok(m) => m,
        Err(refusal) => return refusal.into_response(),
    };
    let (path, _) = match adopted_folder(&app, &monitor, query.folder).await {
        Ok(found) => found,
        Err(refusal) => return refusal.into_response(),
    };
    if let Some(panel) = monitor.panel(&path) {
        return Json(&*panel).into_response();
    }
    // Not read yet (the folder was just added): read it for this answer.
    let provider = Arc::clone(monitor.provider());
    let read = off_runtime(PANEL_TIMEOUT, "reading the panel", move || {
        provider.panel(&path)
    })
    .await;
    match read {
        Ok(panel) => Json(panel).into_response(),
        Err(refusal) => refusal.into_response(),
    }
}

#[derive(Deserialize)]
pub(super) struct RunQuery {
    folder: String,
    run: String,
    #[serde(default)]
    since: usize,
}

pub(super) async fn run_output(
    State(app): State<AppState>,
    Query(query): Query<RunQuery>,
) -> Response {
    let monitor = match monitor(&app) {
        Ok(m) => m,
        Err(refusal) => return refusal.into_response(),
    };
    if !valid_run_id(&query.run) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "run must be 1-128 letters, digits, '-' or '_'",
        );
    }
    let (path, _) = match adopted_folder(&app, &monitor, query.folder).await {
        Ok(found) => found,
        Err(refusal) => return refusal.into_response(),
    };
    let provider = Arc::clone(monitor.provider());
    let (run, since) = (query.run, query.since);
    let read = off_runtime(RUN_OUTPUT_TIMEOUT, "reading the run", move || {
        provider.run_output(&path, &run, since)
    })
    .await;
    match read {
        Ok(Ok(output)) => Json(output).into_response(),
        Ok(Err(message)) => error_response(StatusCode::NOT_FOUND, &message),
        Err(refusal) => refusal.into_response(),
    }
}

#[derive(Deserialize)]
pub(super) struct WorkerRequest {
    folder: String,
    workspace_id: Option<String>,
    cols: Option<u16>,
    rows: Option<u16>,
}

/// The session name a started worker gets.
pub const WORKER_SESSION_NAME: &str = "fw worker";

pub(super) async fn start_worker(
    State(app): State<AppState>,
    super::ApiJson(request): super::ApiJson<WorkerRequest>,
) -> Response {
    let monitor = match monitor(&app) {
        Ok(m) => m,
        Err(refusal) => return refusal.into_response(),
    };
    let (path, workspaces) = match adopted_folder(&app, &monitor, request.folder).await {
        Ok(found) => found,
        Err(refusal) => return refusal.into_response(),
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
    let hub = Arc::clone(&app.hub);
    let provider = Arc::clone(monitor.provider());
    let folder = path.clone();
    let created = off_runtime(Duration::from_secs(10), "starting the worker", move || {
        let command = worker_session_command(&provider.worker_command(&folder));
        hub.create_session(CreateSession {
            workspace_id: workspace.id,
            repo: Some(folder),
            name: Some(WORKER_SESSION_NAME.to_owned()),
            kind: Default::default(),
            command: Some(command),
            cols: request.cols,
            rows: request.rows,
            worktree: None,
        })
    })
    .await;
    match created {
        Ok(Ok(view)) => {
            monitor.refresh(&path);
            (StatusCode::CREATED, Json(view)).into_response()
        }
        Ok(Err(e)) => super::ApiError(e).into_response(),
        Err(refusal) => refusal.into_response(),
    }
}

/// What the events socket relays from the monitor.
pub(super) enum Relay {
    Event(FlywheelEvent),
    /// Some events were missed: the browser re-lists.
    Reset,
}

/// The monitor's next event; pends forever without a monitor (or once
/// it is gone), so the events pump's `select!` simply never picks it.
pub(super) async fn next_event(events: &mut Option<broadcast::Receiver<FlywheelEvent>>) -> Relay {
    loop {
        let Some(receiver) = events.as_mut() else {
            return std::future::pending().await;
        };
        match receiver.recv().await {
            Ok(event) => return Relay::Event(event),
            Err(broadcast::error::RecvError::Lagged(_)) => return Relay::Reset,
            Err(broadcast::error::RecvError::Closed) => *events = None,
        }
    }
}

pub(super) fn event_message(relay: Relay) -> Message {
    let value = match relay {
        Relay::Event(FlywheelEvent::Changed(summary)) => {
            let summary: &FlywheelSummary = &summary;
            json!({ "type": "flywheel", "folder": summary.folder, "summary": summary })
        }
        Relay::Event(FlywheelEvent::Removed(folder)) => {
            json!({ "type": "flywheel", "folder": folder, "summary": null })
        }
        Relay::Reset => json!({ "type": "flywheel_reset" }),
    };
    Message::Text(value.to_string().into())
}
