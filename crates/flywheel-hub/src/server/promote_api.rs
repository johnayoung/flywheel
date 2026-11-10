//! A fan-out and a flywheel task, both ways (see `crate::fanout::promote`):
//!
//! * `POST /api/fanouts/{id}/promote {"task_id"?,"phase"?,"goal"?,
//!   "reference"?,"dry_run"|"confirm"}` -- dry run: `{"plan":{"path",
//!   "relative","contents","phase","new_phase","phases",...},"fingerprint",
//!   "reference","warnings"}`; then `{"confirm":fingerprint}` writes the
//!   task file (uncommitted, in the fan-out's main checkout) ->
//!   `{"written":{...},"plan","warnings","fanout"}`. 409 when the plan
//!   changed since the dry run or the file exists; 404 without the
//!   Flywheel panel or for a repo flywheel does not drive;
//! * `POST /api/flywheel/tasks {"folder","task_id","goal","phase"?,
//!   "prerequisites"?,"checks":[{"run","name"?}],"dry_run"|"confirm"}` --
//!   a new task from scratch, planned and written the same way (into the
//!   project's tasks dir under the flywheel home, never the repo tree);
//!   held-out checks, unknown prerequisites, a used id, or a new phase
//!   name that is not kebab-case are refused (400) with nothing written;
//!   refused to peers;
//! * `GET /api/flywheel/task?folder=P&task=ID` -- the task as a fan-out
//!   seed (held-out graders by name only);
//! * `POST /api/fanouts` with `"from_task"`: seeded server-side
//!   ([`seed_request`]).

use super::{ApiJson, ApiQuery, ApiResult, AppState, blocking, error_response};
use crate::fanout::promote::{self, CreateTaskRequest, PromoteRequest};
use crate::fanout::{CreateFanout, Promotion};
use crate::flywheel::FlywheelProvider;
use crate::hub::HubError;
use crate::state::StateError;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

fn invalid(message: impl Into<String>) -> HubError {
    StateError::Invalid(message.into()).into()
}

fn conflict(message: impl Into<String>) -> HubError {
    StateError::Conflict(message.into()).into()
}

fn not_found(message: impl Into<String>) -> HubError {
    StateError::NotFound(message.into()).into()
}

fn provider(app: &AppState) -> Result<Arc<dyn FlywheelProvider>, HubError> {
    app.options
        .flywheel
        .as_ref()
        .map(|m| Arc::clone(m.provider()))
        .ok_or_else(|| {
            not_found(
                "the Flywheel panel is off on this hub (started with --no-flywheel, or without \
                 the loop's crates): it is what reads and writes flywheel tasks",
            )
        })
}

fn require_adopted(
    provider: &dyn FlywheelProvider,
    folder: &std::path::Path,
) -> Result<(), HubError> {
    if provider.adopted(folder) {
        return Ok(());
    }
    Err(not_found(format!(
        "{} is not a flywheel project (no project id flywheel knows): run `fw init` there first",
        folder.display()
    )))
}

/// Promote a fan-out to a flywheel task (dry run, then confirm).
pub(super) async fn promote(
    State(app): State<AppState>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<PromoteRequest>,
) -> ApiResult<Response> {
    let Some(engine) = app.options.fanouts.clone() else {
        return Ok(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "fan-outs are off on this hub (it was started without them)",
        ));
    };
    let provider = provider(&app)?;
    let monitor = app.options.flywheel.clone();
    let answer = blocking(move || {
        let fanout = engine.record(&id)?;
        require_adopted(provider.as_ref(), &fanout.repo)?;
        if let Some(refusal) = engine.promotion_refusal(&fanout) {
            return Err(conflict(refusal));
        }
        let leader = engine.view(&id)?.leader;
        let drafted = promote::draft(&fanout, &body, leader.as_deref()).map_err(invalid)?;
        let plan = provider
            .plan_task(&fanout.repo, &drafted.draft)
            .map_err(invalid)?;
        let fingerprint = promote::fingerprint(&plan);
        if body.dry_run {
            return Ok(json!({
                "plan": plan,
                "fingerprint": fingerprint,
                "reference": drafted.reference,
                "warnings": drafted.warnings,
            }));
        }
        match body.confirm.as_deref() {
            None => {
                return Err(invalid(
                    "pass dry_run to see what is written, then confirm with its fingerprint",
                ));
            }
            Some(confirm) if confirm != fingerprint => {
                return Err(conflict(
                    "what would be written changed since the dry run: review it again",
                ));
            }
            Some(_) => {}
        }
        let written = provider
            .write_task(&fanout.repo, &drafted.draft, &plan.contents)
            .map_err(conflict)?;
        eprintln!(
            "fw hub: fan-out {} ({id}) promoted to flywheel task {} ({})",
            fanout.name,
            written.task_id,
            written.path.display()
        );
        engine.note_promotion(
            &id,
            Promotion {
                task_id: written.task_id.clone(),
                phase: written.phase.clone(),
                path: written.path.clone(),
                reference: drafted.reference.clone(),
                at: Utc::now(),
            },
        )?;
        if let Some(monitor) = &monitor {
            monitor.refresh(&fanout.repo);
        }
        Ok(json!({
            "written": written,
            "plan": plan,
            "warnings": drafted.warnings,
            "fanout": engine.view(&id)?,
        }))
    })
    .await?;
    Ok(Json(answer).into_response())
}

/// Write a new task (dry run, then confirm).
pub(super) async fn create_task(
    State(app): State<AppState>,
    ApiJson(body): ApiJson<CreateTaskRequest>,
) -> ApiResult<Response> {
    let provider = provider(&app)?;
    let monitor = app.options.flywheel.clone();
    let answer = blocking(move || {
        let folder = workspace_folder(&app, &body.folder)?;
        require_adopted(provider.as_ref(), &folder)?;
        let draft = promote::create_draft(&body).map_err(invalid)?;
        let plan = provider.plan_task(&folder, &draft).map_err(invalid)?;
        if plan.new_phase && !promote::kebab_case(&plan.phase) {
            return Err(invalid(format!(
                "a new phase name must be kebab-case (lowercase letters and digits in \
                 '-'-separated words), not {:?}; or name an active phase: {}",
                plan.phase,
                if plan.phases.is_empty() {
                    "none yet".to_owned()
                } else {
                    plan.phases.join(", ")
                }
            )));
        }
        let fingerprint = promote::fingerprint(&plan);
        if body.dry_run {
            return Ok(json!({ "plan": plan, "fingerprint": fingerprint }));
        }
        match body.confirm.as_deref() {
            None => {
                return Err(invalid(
                    "pass dry_run to see what is written, then confirm with its fingerprint",
                ));
            }
            Some(confirm) if confirm != fingerprint => {
                return Err(conflict(
                    "what would be written changed since the dry run: review it again",
                ));
            }
            Some(_) => {}
        }
        let written = provider
            .write_task(&folder, &draft, &plan.contents)
            .map_err(conflict)?;
        eprintln!(
            "fw hub: new flywheel task {} ({})",
            written.task_id,
            written.path.display()
        );
        if let Some(monitor) = &monitor {
            monitor.refresh(&folder);
        }
        Ok(json!({ "written": written, "plan": plan }))
    })
    .await?;
    let status = if answer.get("written").is_some() {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(answer)).into_response())
}

#[derive(Deserialize)]
pub(super) struct TaskQuery {
    folder: PathBuf,
    task: String,
}

/// A workspace folder (canonical) that holds `folder`.
fn workspace_folder(app: &AppState, folder: &std::path::Path) -> Result<PathBuf, HubError> {
    let canonical = folder
        .canonicalize()
        .map_err(|e| invalid(format!("folder {}: {e}", folder.display())))?;
    if app
        .hub
        .workspaces()
        .iter()
        .any(|w| w.has_folder(&canonical))
    {
        Ok(canonical)
    } else {
        Err(not_found(format!(
            "{} is not a folder of any workspace",
            canonical.display()
        )))
    }
}

/// A flywheel task as a fan-out seed, for a launch form.
pub(super) async fn task_seed(
    State(app): State<AppState>,
    ApiQuery(query): ApiQuery<TaskQuery>,
) -> ApiResult<Response> {
    let provider = provider(&app)?;
    let seed = blocking(move || {
        let folder = workspace_folder(&app, &query.folder)?;
        require_adopted(provider.as_ref(), &folder)?;
        provider
            .task_seed(&folder, query.task.trim())
            .map_err(not_found)
    })
    .await?;
    Ok(Json(seed.redacted()).into_response())
}

/// `request` seeded from its `from_task` (off the runtime: the provider
/// reads the task tree and the held-out registry); as is without one.
pub(super) async fn seed_request(app: &AppState, request: CreateFanout) -> ApiResult<CreateFanout> {
    let Some(task_id) = request.from_task.clone() else {
        return Ok(request);
    };
    let provider = provider(app)?;
    let app = app.clone();
    blocking(move || {
        let folder = match &request.folder {
            Some(folder) => workspace_folder(&app, folder)?,
            None => app
                .hub
                .workspace(&request.workspace_id)?
                .folders
                .iter()
                .find(|f| f.git && !f.missing && provider.adopted(&f.path))
                .map(|f| f.path.clone())
                .ok_or_else(|| {
                    invalid("the workspace has no flywheel repo to take the task from")
                })?,
        };
        require_adopted(provider.as_ref(), &folder)?;
        let seed = provider
            .task_seed(&folder, task_id.trim())
            .map_err(invalid)?;
        promote::seeded(request, &seed).map_err(invalid)
    })
    .await
}
