//! Producers for the steering verbs (interrupt / steer / set-model /
//! approve / reject): persist one control command per invocation.

use super::{RunStore, WorkflowError};
use crate::invoker_client::{
    CONTROL_COMMAND_APPROVE, CONTROL_COMMAND_REJECT, GateDecisionError, enqueue_gate_decision,
};
use crate::lifecycle::Status;
use crate::store::JsonObject;
use crate::store_sqlite::SqliteStore;
use chrono::Utc;
use std::path::Path;
use std::sync::Arc;

// --- Steering / control commands ---------------------------------------------------
//
// Producers for the cross-process control plane. Each verb enqueues exactly
// one row into `control_commands` keyed by `run_id`; the in-process watcher
// inside the worker's invoker claims and applies it on its next tick. The CLI
// never talks to the live session directly -- the store is the channel -- so
// steering works against a detached worker daemon. A command is persisted
// unconditionally; if the named run is not currently in-flight the row sits
// pending and is recorded as stale per claim semantics.

/// Persist one control command into the store and report the id.
///
/// Returns `0` after printing the enqueue receipt (`enqueued #<id> kind=...`)
/// plus, when the lifecycle is not currently in-flight, a stderr note
/// explaining the row stays pending per claim semantics. An unknown `run_id`
/// is a producer-side error: exit code `2` with a clear message and nothing
/// enqueued. A store failure prints `error: ...` and returns `1`.
///
/// `store` injects the backend the command lands in; `None` opens the
/// [`SqliteStore`] at `db_path` (creating its parent directory) and closes
/// it on exit. `approve` / `reject` go through
/// [`enqueue_gate_decision`]: bound to the gate parked right now, and refused
/// with exit `1` (nothing enqueued) when the run has no parked gate or a
/// `--gate` / `--attempt` pin names a different one. Every
/// other verb uses the standard `running` / `validating` in-flight note.
pub fn enqueue_control_command(
    db_path: &Path,
    run_id: &str,
    kind: &str,
    payload: JsonObject,
    store: Option<Arc<dyn RunStore>>,
) -> i32 {
    match enqueue_control_command_inner(db_path, run_id, kind, payload, store) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err}");
            1
        }
    }
}

fn enqueue_control_command_inner(
    db_path: &Path,
    run_id: &str,
    kind: &str,
    payload: JsonObject,
    store: Option<Arc<dyn RunStore>>,
) -> Result<i32, WorkflowError> {
    let mut owned: Option<Arc<SqliteStore>> = None;
    let backend: Arc<dyn RunStore> = match store {
        Some(store) => store,
        None => {
            if let Some(parent) = db_path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)?;
            }
            let store = Arc::new(SqliteStore::open(db_path)?);
            owned = Some(store.clone());
            store
        }
    };
    let gate_decision = kind == CONTROL_COMMAND_APPROVE || kind == CONTROL_COMMAND_REJECT;
    let outcome = (|| -> Result<(Status, Option<i64>), GateDecisionError> {
        if gate_decision {
            // approve/reject bind to the gate parked right now; a run with no
            // parked gate refuses rather than queueing a decision ahead of it.
            let (record, binding) =
                enqueue_gate_decision(backend.as_ref(), run_id, kind, payload, Utc::now())?;
            eprintln!("  bound to manual gate {binding}");
            return Ok((Status::AwaitingApproval, record.id));
        }
        let lifecycle = backend
            .load_lifecycle(run_id)?
            .ok_or_else(|| GateDecisionError::UnknownRun(run_id.to_owned()))?;
        let record = backend.enqueue_command(run_id, kind, payload, Utc::now())?;
        Ok((lifecycle.status, record.id))
    })();
    drop(backend);
    if let Some(store) = owned
        && let Ok(store) = Arc::try_unwrap(store)
    {
        store.close()?;
    }
    let (status, id) = match outcome {
        Ok(enqueued) => enqueued,
        Err(GateDecisionError::UnknownRun(_)) => {
            eprintln!("error: run {run_id:?} is unknown to this store; no command enqueued");
            return Ok(2);
        }
        Err(
            err @ (GateDecisionError::NotParked { .. } | GateDecisionError::GateMismatch { .. }),
        ) => {
            eprintln!("error: {err}");
            return Ok(1);
        }
        Err(GateDecisionError::Store(err)) => return Err(err.into()),
    };
    let id = id.map_or_else(|| "None".to_owned(), |id| id.to_string());
    println!("enqueued #{id} kind={kind} run_id={run_id}");
    if !gate_decision && !matches!(status, Status::Running | Status::Validating) {
        eprintln!(
            "  note: run {run_id} is not in-flight (status={}); the command stays pending \
             and is recorded as stale per claim semantics",
            status.as_str()
        );
    }
    Ok(0)
}
