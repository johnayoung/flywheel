//! Store-backed lifecycle queries for the single-task workflow: whether a
//! task is already done, and finding/finalizing stranded runs.

use crate::harness::{FinalizeStrandedOptions, SharedSink, finalize_stranded_lifecycle};
use crate::lifecycle::Status;
use crate::store::{HarnessStore, LifecycleFilter, LifecycleStore, StoreError};
use std::sync::Arc;

// --- Status queries ---------------------------------------------------------

/// Whether `task_id` has at least one `done` lifecycle in `store`.
pub fn has_done_lifecycle(store: &dyn LifecycleStore, task_id: &str) -> Result<bool, StoreError> {
    let filter = LifecycleFilter::default()
        .with_statuses([Status::Done])
        .with_task_id(task_id);
    Ok(!store.list_lifecycles(&filter)?.is_empty())
}

// --- Stranded-lifecycle recovery -------------------------------------------

/// The statuses a lifecycle is considered stranded in once its worker is gone.
pub const STRANDED_STATUSES: [Status; 2] = [Status::Running, Status::Validating];

/// Return run_ids whose lifecycle is mid-attempt with no live worker.
///
/// A run is considered stranded when its status sits in `running` or
/// `validating` after the worker that owned it exited -- there is no
/// process-level liveness check available here, so the caller uses this only
/// at boundaries where it knows no harness is currently running that
/// lifecycle (worker start, post-interrupt cleanup, `recover`). Ordered
/// oldest-updated first (`list_lifecycles` orders newest first; reversed).
pub fn stranded_run_ids(
    store: &dyn LifecycleStore,
    task_id: Option<&str>,
) -> Result<Vec<String>, StoreError> {
    let mut filter = LifecycleFilter::default().with_statuses(STRANDED_STATUSES);
    if let Some(task_id) = task_id {
        filter = filter.with_task_id(task_id);
    }
    let lifecycles = store.list_lifecycles(&filter)?;
    Ok(lifecycles.into_iter().rev().map(|lc| lc.run_id).collect())
}

/// Finalize every stranded lifecycle, optionally filtered by `task_id`.
///
/// Delegates to [`finalize_stranded_lifecycle`] for each match; returns the
/// run_ids that were actually finalized (i.e. were in `running` /
/// `validating` at the time of the call).
pub fn recover_stranded_lifecycles(
    store: Arc<dyn HarnessStore>,
    task_id: Option<&str>,
    sink: Option<SharedSink>,
) -> Result<Vec<String>, StoreError> {
    let mut finalized = Vec::new();
    for run_id in stranded_run_ids(store.as_ref(), task_id)? {
        let mut options = FinalizeStrandedOptions::default();
        if let Some(sink) = &sink {
            options = options.with_sink(sink.clone());
        }
        if finalize_stranded_lifecycle(store.clone(), &run_id, options)? {
            finalized.push(run_id);
        }
    }
    Ok(finalized)
}
