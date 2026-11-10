//! The Flywheel panel's API (see `crate::flywheel`):
//!
//! * `GET /api/flywheel` -- `{"enabled", "folders": [summary...]}`;
//! * `GET /api/flywheel/panel?folder=P` -- one folder's whole panel, its
//!   pending tasks a running scoped worker will run marked `queued_in`;
//! * `GET /api/flywheel/run?folder=P&run=R[&since=N]` -- a run's `fw show`
//!   summary and its transcript from entry `N` on;
//! * `GET /api/flywheel/history?folder=P&task=T` -- every recorded run of
//!   task `T` (never windowed) with each attempt's outcome and error, and
//!   its checks (held-out ones by name only);
//! * `POST /api/flywheel/worker {"folder", "workspace_id"?, "another"?,
//!   "tasks"?, "with_prereqs"?, "cols"?, "rows"?}` -- start `fw worker`
//!   (`fw worker --task ID ...` when `tasks` names any) in a new hub shell
//!   session there; 409 (with the running session) while a worker this
//!   hub started still runs in the folder, unless `another` is true;
//! * `POST /api/flywheel/steer/say {"folder", "run", "message"}` and
//!   `POST /api/flywheel/steer/interrupt {"folder", "run"}` -- enqueue one
//!   `say` / `interrupt` control command for a running or validating run
//!   (409, nothing enqueued, for any other state); refused to peers;
//! * `{"type":"flywheel"}` messages on the events socket.
//!
//! Only folders of the hub's workspaces are served, and only adopted ones.

use super::ApiQuery;
use super::{AppState, error_response};
use crate::flywheel::{
    FlywheelEvent, FlywheelMonitor, FlywheelSummary, MAX_SAY_CHARS, Steer, SteerRefused,
    WorkerScope, mark_queued, runs_worker, valid_run_id, worker_session_command,
    worker_session_name,
};
use crate::hub::{CreateSession, Hub};
use crate::state::Workspace;
use axum::Json;
use axum::extract::State;
use axum::extract::ws::Message;
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
pub(super) struct Refusal {
    status: StatusCode,
    message: String,
}

impl Refusal {
    pub(super) fn new(status: StatusCode, message: impl Into<String>) -> Self {
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

/// The answer while an earlier read with the same key is still blocked
/// (on a slow or unreachable store).
pub(super) fn still_reading(what: &str) -> Response {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        &format!("an earlier {what} is still waiting on the store; try again shortly"),
    )
}

pub(super) fn monitor(app: &AppState) -> Result<Arc<FlywheelMonitor>, Refusal> {
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

pub(super) async fn off_runtime<T: Send + 'static>(
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
pub(super) async fn adopted_folder(
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
                    "{} is not a flywheel project (no project id flywheel knows; run `fw init` there)",
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
    ApiQuery(query): ApiQuery<PanelQuery>,
) -> Response {
    let monitor = match monitor(&app) {
        Ok(m) => m,
        Err(refusal) => return refusal.into_response(),
    };
    let (path, _) = match adopted_folder(&app, &monitor, query.folder).await {
        Ok(found) => found,
        Err(refusal) => return refusal.into_response(),
    };
    let panel = match monitor.panel(&path) {
        Some(panel) => (*panel).clone(),
        None => {
            // Not read yet (the folder was just added): read it for this
            // answer.
            let Some(guard) = monitor.begin_read(format!("panel\0{}", path.display())) else {
                return still_reading("panel read");
            };
            let provider = Arc::clone(monitor.provider());
            let folder = path.clone();
            let read = off_runtime(PANEL_TIMEOUT, "reading the panel", move || {
                let panel = provider.panel(&folder);
                drop(guard);
                panel
            })
            .await;
            match read {
                Ok(panel) => panel,
                Err(refusal) => return refusal.into_response(),
            }
        }
    };
    // Which pending tasks a running scoped worker here will run (read
    // from its process, so off the runtime).
    let hub = Arc::clone(&app.hub);
    let marked = off_runtime(Duration::from_secs(5), "reading the workers", move || {
        let mut panel = panel;
        mark_queued(&mut panel, &path, &hub.sessions(None));
        if let Some(intake) = panel.intake.take() {
            panel.intake = Some(crate::autopilot::with_history(
                intake,
                hub.activity(),
                &path,
            ));
        }
        panel
    })
    .await;
    match marked {
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
    ApiQuery(query): ApiQuery<RunQuery>,
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
    let Some(guard) = monitor.begin_read(format!("run\0{}\0{}", path.display(), query.run)) else {
        return still_reading("read of this run");
    };
    let provider = Arc::clone(monitor.provider());
    let (run, since) = (query.run, query.since);
    let read = off_runtime(RUN_OUTPUT_TIMEOUT, "reading the run", move || {
        let output = provider.run_output(&path, &run, since);
        drop(guard);
        output
    })
    .await;
    match read {
        Ok(Ok(output)) => Json(output).into_response(),
        Ok(Err(message)) => error_response(StatusCode::NOT_FOUND, &message),
        Err(refusal) => refusal.into_response(),
    }
}

#[derive(Deserialize)]
pub(super) struct HistoryQuery {
    folder: String,
    task: String,
}

pub(super) async fn task_history(
    State(app): State<AppState>,
    ApiQuery(query): ApiQuery<HistoryQuery>,
) -> Response {
    let monitor = match monitor(&app) {
        Ok(m) => m,
        Err(refusal) => return refusal.into_response(),
    };
    let task = query.task.trim().to_owned();
    if let Err(why) = WorkerScope::new(vec![task.clone()], false) {
        return error_response(StatusCode::BAD_REQUEST, &why);
    }
    let (path, _) = match adopted_folder(&app, &monitor, query.folder).await {
        Ok(found) => found,
        Err(refusal) => return refusal.into_response(),
    };
    let Some(guard) = monitor.begin_read(format!("history\0{}\0{task}", path.display())) else {
        return still_reading("read of this task's runs");
    };
    let provider = Arc::clone(monitor.provider());
    let read = off_runtime(RUN_OUTPUT_TIMEOUT, "reading the task's runs", move || {
        let history = provider.task_history(&path, &task);
        drop(guard);
        history
    })
    .await;
    match read {
        Ok(Ok(history)) => Json(history).into_response(),
        Ok(Err(message)) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &message),
        Err(refusal) => refusal.into_response(),
    }
}

#[derive(Deserialize)]
pub(super) struct WorkerRequest {
    folder: String,
    workspace_id: Option<String>,
    /// Start one even though a worker this hub started still runs here.
    #[serde(default)]
    another: bool,
    /// Run only these tasks (`fw worker --task ID ...`); empty drains the
    /// whole queue.
    #[serde(default)]
    tasks: Vec<String>,
    /// With `tasks`: also run their not-done prerequisites, first.
    #[serde(default)]
    with_prereqs: bool,
    cols: Option<u16>,
    rows: Option<u16>,
}

/// Why a worker start did not create a session.
enum StartRefused {
    Running { id: String, name: String },
    Hub(crate::hub::HubError),
}

pub(super) async fn start_worker(
    State(app): State<AppState>,
    super::ApiJson(mut request): super::ApiJson<WorkerRequest>,
) -> Response {
    let monitor = match monitor(&app) {
        Ok(m) => m,
        Err(refusal) => return refusal.into_response(),
    };
    let scope = match WorkerScope::new(std::mem::take(&mut request.tasks), request.with_prereqs) {
        Ok(scope) => scope,
        Err(why) => return error_response(StatusCode::BAD_REQUEST, &why),
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
    let starter = Arc::clone(&monitor);
    let folder = path.clone();
    // An action, not a read: no timeout. Giving up early would answer
    // "failed" for a session that still appears, inviting a duplicate.
    let created = tokio::task::spawn_blocking(move || {
        // Check-then-create under one lock: concurrent starts serialize.
        let _starting = starter.starting();
        let sessions = hub.sessions(None);
        if !request.another
            && let Some(running) = sessions.iter().find(|s| runs_worker(s, &folder))
        {
            return Err(StartRefused::Running {
                id: running.record.id.clone(),
                name: running.record.name.clone(),
            });
        }
        let name = worker_session_name(&scope, sessions.iter().map(|s| s.record.name.as_str()));
        let command = worker_session_command(&starter.provider().worker_command(&folder, &scope));
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
    })
    .await;
    match created {
        Ok(Ok(view)) => {
            monitor.refresh(&path);
            (StatusCode::CREATED, Json(view)).into_response()
        }
        Ok(Err(StartRefused::Running { id, name })) => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!(
                    "a worker is already running in {} (session {name:?}); \
                     start another only to run tasks in parallel with it",
                    path.display()
                ),
                "running": { "id": id, "name": name },
            })),
        )
            .into_response(),
        Ok(Err(StartRefused::Hub(e))) => super::ApiError(e).into_response(),
        Err(join) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("starting the worker failed: {join}"),
        ),
    }
}

#[derive(Deserialize)]
pub(super) struct SayRequest {
    folder: String,
    run: String,
    message: String,
}

#[derive(Deserialize)]
pub(super) struct InterruptRequest {
    folder: String,
    run: String,
}

pub(super) async fn say(
    State(app): State<AppState>,
    super::ApiJson(request): super::ApiJson<SayRequest>,
) -> Response {
    if request.message.trim().is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "message must be non-empty");
    }
    if request.message.chars().count() > MAX_SAY_CHARS {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!("message is limited to {MAX_SAY_CHARS} characters"),
        );
    }
    let steer = Steer::Say {
        text: request.message,
    };
    steer_run(app, request.folder, request.run, steer).await
}

pub(super) async fn interrupt(
    State(app): State<AppState>,
    super::ApiJson(request): super::ApiJson<InterruptRequest>,
) -> Response {
    steer_run(app, request.folder, request.run, Steer::Interrupt).await
}

/// Enqueue `steer` for live run `run` of `folder`.
async fn steer_run(app: AppState, folder: String, run: String, steer: Steer) -> Response {
    let monitor = match monitor(&app) {
        Ok(m) => m,
        Err(refusal) => return refusal.into_response(),
    };
    if !valid_run_id(&run) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "run must be 1-128 letters, digits, '-' or '_'",
        );
    }
    let (path, _) = match adopted_folder(&app, &monitor, folder).await {
        Ok(found) => found,
        Err(refusal) => return refusal.into_response(),
    };
    let provider = Arc::clone(monitor.provider());
    let folder = path.clone();
    // An action: no timeout (a command enqueued after a give-up would read
    // as failed, inviting a duplicate).
    let steered =
        tokio::task::spawn_blocking(move || provider.steer_run(&folder, &run, &steer)).await;
    match steered {
        Ok(Ok(done)) => {
            monitor.refresh(&path);
            (StatusCode::CREATED, Json(done)).into_response()
        }
        Ok(Err(SteerRefused::UnknownRun(m))) => error_response(StatusCode::NOT_FOUND, &m),
        Ok(Err(SteerRefused::NotLive(m))) => error_response(StatusCode::CONFLICT, &m),
        Ok(Err(SteerRefused::Store(m))) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &m),
        Err(join) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("steering the run failed: {join}"),
        ),
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
