//! The loop's background and bookkeeping passes: the steering reconciler,
//! the expired-lease sweep, the entry-time stranded recovery, the
//! `source_syncs` catalog pass, and the per-pass graph-snapshot assembly.

use crate::claims::{
    ClaimStore, ClaimStoreError, GraphSnapshotItem, SourceSyncRecord, WorkItemObservation,
};
use crate::rollup::TaskState;
use crate::sources::{LogSink, WorkItem, WorkSource};
use crate::work_graph::{ReadySetQuery, WorkGraph};
use chrono::{DateTime, Utc};
use flywheel_core::harness::{FinalizeStrandedOptions, SharedSink, finalize_stranded_lifecycle};
use flywheel_core::invoker_client::{CONTROL_COMMAND_INTERRUPT, Clock};
use flywheel_core::lifecycle::Status;
use flywheel_core::store::{HarnessStore, LifecycleFilter, StoreError};
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use super::OrchestrateError;
use super::prereqs::stranded_run_ids;

/// Emit one line to the optional log.
pub(super) fn emit(log: Option<&LogSink>, line: impl AsRef<str>) {
    if let Some(log) = log {
        log(line.as_ref());
    }
}

/// `(run_id, task_id)` for every lifecycle currently in flight, sorted by
/// run id.
///
/// Only `running` / `validating` qualify: those have a live worker session
/// an `interrupt` command can reach. `AWAITING_APPROVAL` is deliberately
/// excluded -- a parked gate has no session to interrupt and its
/// disposition belongs to the approve / reject resolver.
fn live_run_rows(control: &dyn HarnessStore) -> Result<Vec<(String, String)>, StoreError> {
    let filter = LifecycleFilter::default().with_statuses([Status::Running, Status::Validating]);
    let mut rows: Vec<(String, String)> = control
        .list_lifecycles(&filter)?
        .into_iter()
        .map(|lc| (lc.run_id, lc.task_id))
        .collect();
    rows.sort();
    Ok(rows)
}

/// Enqueue `interrupt` for live runs whose item the source dropped.
///
/// `wanted_task_ids` is the current listing; any in-flight run whose task id
/// is absent gets one `interrupt` control command (the store-routed path:
/// the run's own watcher applies it, so this works for peer workers' runs
/// too). `already_signaled` is the caller-owned dedup set: a run is signaled
/// at most once per session even though it stays `running` until the
/// watcher acts. Returns the run ids newly signaled.
pub fn reconcile_live_runs(
    control: &dyn HarnessStore,
    wanted_task_ids: &BTreeSet<String>,
    already_signaled: &mut HashSet<String>,
    now: DateTime<Utc>,
) -> Result<Vec<String>, StoreError> {
    let mut signaled = Vec::new();
    for (run_id, task_id) in live_run_rows(control)? {
        if wanted_task_ids.contains(&task_id) || already_signaled.contains(&run_id) {
            continue;
        }
        let mut payload = Map::new();
        payload.insert("origin".into(), Value::String("source-reconciler".into()));
        payload.insert(
            "reason".into(),
            Value::String(format!(
                "task {task_id:?} is no longer listed by the work source"
            )),
        );
        control.enqueue_command(&run_id, CONTROL_COMMAND_INTERRUPT, payload, now)?;
        already_signaled.insert(run_id.clone());
        signaled.push(run_id);
    }
    Ok(signaled)
}

/// Tick [`reconcile_live_runs`] every `interval` seconds (the steering
/// bridge). Runs as a sibling tokio task while the loop awaits drives.
///
/// Failure posture: a listing failure NEVER interrupts anything -- a
/// tracker hiccup must not be read as "all work vanished" -- and a store
/// error skips the tick. Both are logged and retried next tick; the task
/// only exits by abort.
pub(super) async fn source_reconcile_loop(
    source: Arc<dyn WorkSource>,
    control: Arc<dyn HarnessStore>,
    interval: f64,
    now: Clock,
    log: Option<LogSink>,
) {
    let mut already_signaled: HashSet<String> = HashSet::new();
    loop {
        tokio::time::sleep(std::time::Duration::from_secs_f64(interval)).await;
        // The wanted set is every id the source lists this tick -- a listed
        // item is never interrupted, even one the graph would isolate for a
        // structural defect (it is still listed work).
        let wanted: BTreeSet<String> = match source.list_work() {
            Ok(items) => items.into_iter().map(|item| item.task.id).collect(),
            Err(err) => {
                emit(
                    log.as_ref(),
                    format!(
                        "[orchestrate] steering: work-source listing failed (WorkSourceError: {err}); no runs interrupted, retrying next tick"
                    ),
                );
                continue;
            }
        };
        let newly = match reconcile_live_runs(
            control.as_ref(),
            &wanted,
            &mut already_signaled,
            now(),
        ) {
            Ok(newly) => newly,
            Err(err) => {
                emit(
                    log.as_ref(),
                    format!(
                        "[orchestrate] steering: reconcile failed ({}: {err}); retrying next tick",
                        flywheel_core::harness::store_error_type(&err)
                    ),
                );
                continue;
            }
        };
        for run_id in newly {
            emit(
                log.as_ref(),
                format!(
                    "[orchestrate] steering: run {run_id}'s item vanished from the work source; interrupt enqueued"
                ),
            );
        }
    }
}

/// Run one `list_work()` pass and persist it as a `source_syncs` run.
///
/// On success it upserts every observed item, replaces each item's
/// dependency edges, marks previously-seen-but-now-absent items disappeared,
/// and finishes `status = "ok"`. On a failed listing it finishes `"error"`
/// with a non-empty error and marks NOTHING disappeared -- a tracker hiccup
/// is never read as task disappearance.
pub fn sync_work_source(
    source: &dyn WorkSource,
    store: &dyn ClaimStore,
    now: DateTime<Utc>,
) -> Result<SourceSyncRecord, ClaimStoreError> {
    let sync_id =
        store.record_source_sync_start(source.source_kind(), &source.source_name(), now)?;
    let items = match source.list_work() {
        Ok(items) => items,
        Err(err) => {
            store.record_source_sync_finish(
                sync_id,
                "error",
                0,
                Some(&format!("WorkSourceError: {err}")),
                now,
            )?;
            return store
                .load_source_sync(sync_id)?
                .ok_or_else(|| ClaimStoreError::Backend("source_syncs row vanished".into()));
        }
    };
    for item in &items {
        store.upsert_work_item(&observation_for(item), now)?;
        store.replace_work_item_dependencies(&item.task.id, &item.prerequisites, now)?;
    }
    let observed: Vec<String> = items.iter().map(|item| item.task.id.clone()).collect();
    store.mark_work_items_disappeared(&observed, now)?;
    store.record_source_sync_finish(sync_id, "ok", items.len() as i64, None, now)?;
    store
        .load_source_sync(sync_id)?
        .ok_or_else(|| ClaimStoreError::Backend("source_syncs row vanished".into()))
}

fn observation_for(item: &WorkItem) -> WorkItemObservation {
    WorkItemObservation {
        source_kind: item.source_kind.clone(),
        source_ref: Some(item.source_ref.clone()),
        source_url: item.source_url.clone(),
        source_version: item.source_version.clone(),
        priority: item.priority,
        required_capabilities: item.required_capabilities.clone(),
        conflict_keys: item.conflict_keys.clone(),
        ..WorkItemObservation::for_task(&item.task)
    }
}

/// Materialize each pass work item's full cross-section for one snapshot.
///
/// A pure read: provenance and scheduling metadata off the pass's items,
/// lifecycle `state` and `ready` membership off the same `states` and
/// `ready_set` the scheduler grades eligibility from, `claim_holder` off the
/// live claim store, `resolved_prerequisites` off the validated graph. It
/// observes the scheduler's inputs and never mutates them.
pub fn assemble_graph_snapshot_items(
    items: &[WorkItem],
    graph: &WorkGraph,
    states: &HashMap<String, TaskState>,
    worker_capabilities: &BTreeSet<String>,
    claims: &dyn ClaimStore,
    satisfied_prerequisites: &BTreeSet<String>,
) -> Result<Vec<GraphSnapshotItem>, ClaimStoreError> {
    let state_values: HashMap<String, &'static str> = states
        .iter()
        .map(|(id, state)| (id.clone(), state.as_str()))
        .collect();
    let query = ReadySetQuery::new()
        .with_worker_capabilities(worker_capabilities.iter().cloned())
        .with_satisfied_prerequisites(satisfied_prerequisites.iter().cloned());
    let ready_ids: HashSet<String> = graph
        .ready_set(&state_values, &query)
        .into_iter()
        .map(|item| item.task.id.clone())
        .collect();
    let holders: HashMap<String, String> = claims
        .list_claims()?
        .into_iter()
        .map(|claim| (claim.task_id, claim.worker_id))
        .collect();
    Ok(items
        .iter()
        .map(|item| {
            let task_id = item.task.id.clone();
            GraphSnapshotItem {
                source_kind: item.source_kind.clone(),
                source_ref: Some(item.source_ref.clone()),
                source_url: item.source_url.clone(),
                source_version: item.source_version.clone(),
                priority: item.priority,
                required_capabilities: item.required_capabilities.clone(),
                conflict_keys: item.conflict_keys.clone(),
                state: states
                    .get(&task_id)
                    .map(|s| s.as_str().to_owned())
                    .unwrap_or_default(),
                ready: ready_ids.contains(&task_id),
                claim_holder: holders.get(&task_id).cloned(),
                resolved_prerequisites: graph.resolved_prerequisites(&task_id),
                task_id,
            }
        })
        .collect())
}

/// Finalize stranded lifecycles whose task has no live owner.
///
/// A lifecycle stuck in `running` / `validating` is stranded only if no
/// worker is actually running it; with peers, a peer's live run looks
/// identical. The claim is the liveness signal: acquiring a task's claim
/// proves no live worker holds it, so its stranded lifecycle is safe to
/// finalize. A task a peer actively runs cannot be claimed and is left
/// untouched. Keyless acquires (bookkeeping never queues behind a shared
/// file key).
pub fn recover_claimable_stranded(
    control: &Arc<dyn HarnessStore>,
    claims: &dyn ClaimStore,
    worker_id: &str,
    lease_seconds: f64,
    steal_grace_seconds: f64,
    now: &Clock,
    sink: Option<SharedSink>,
) -> Result<Vec<String>, OrchestrateError> {
    let mut recovered = Vec::new();
    for run_id in stranded_run_ids(control.as_ref(), None)? {
        let Some(lifecycle) = control.load_lifecycle(&run_id)? else {
            continue;
        };
        let Some(claim) = super::acquire_with_grace(
            claims,
            &lifecycle.task_id,
            worker_id,
            now(),
            lease_seconds,
            &BTreeSet::new(),
            steal_grace_seconds,
        )?
        else {
            // A live peer owns this task; its run is not stranded.
            continue;
        };
        let options = FinalizeStrandedOptions {
            sink: sink.clone(),
            now: Some(now.clone()),
            ..FinalizeStrandedOptions::default()
        };
        let finalized = finalize_stranded_lifecycle(control.clone(), &run_id, options);
        claims.release_claim(&claim, Some(now()))?;
        if finalized? {
            recovered.push(run_id);
        }
    }
    Ok(recovered)
}

/// One expired-lease sweep: finalize claimable stranded lifecycles, then
/// reap any remaining lapsed claim rows (spec 00069, criteria #1/#2).
///
/// Returns `(recovered_run_ids, released_task_ids)`.
pub fn sweep_expired_leases(
    control: &Arc<dyn HarnessStore>,
    claims: &dyn ClaimStore,
    worker_id: &str,
    lease_seconds: f64,
    steal_grace_seconds: f64,
    now: &Clock,
    sink: Option<SharedSink>,
) -> Result<(Vec<String>, Vec<String>), OrchestrateError> {
    let recovered = recover_claimable_stranded(
        control,
        claims,
        worker_id,
        lease_seconds,
        steal_grace_seconds,
        now,
        sink,
    )?;
    let released = claims.sweep_expired_claims(now())?;
    Ok((recovered, released))
}

/// Tick [`sweep_expired_leases`] every `interval` seconds while the loop
/// drives. A store error skips the tick and is retried; the task only
/// exits by abort.
#[allow(clippy::too_many_arguments)]
pub(super) async fn lease_sweep_loop(
    control: Arc<dyn HarnessStore>,
    claims: Arc<dyn ClaimStore>,
    worker_id: String,
    lease_seconds: f64,
    steal_grace_seconds: f64,
    interval: f64,
    now: Clock,
    sink: Option<SharedSink>,
    log: Option<LogSink>,
) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs_f64(interval)).await;
        let swept = sweep_expired_leases(
            &control,
            claims.as_ref(),
            &worker_id,
            lease_seconds,
            steal_grace_seconds,
            &now,
            sink.clone(),
        );
        let (recovered, released) = match swept {
            Ok(swept) => swept,
            Err(err) => {
                emit(
                    log.as_ref(),
                    format!("[orchestrate] lease-sweep failed ({err}); retrying next tick"),
                );
                continue;
            }
        };
        for run_id in recovered {
            emit(
                log.as_ref(),
                format!(
                    "[orchestrate] lease-sweep: finalized stranded run {run_id}; its task is eligible again"
                ),
            );
        }
        for task_id in released {
            emit(
                log.as_ref(),
                format!("[orchestrate] lease-sweep: released lapsed lease on task {task_id:?}"),
            );
        }
    }
}
