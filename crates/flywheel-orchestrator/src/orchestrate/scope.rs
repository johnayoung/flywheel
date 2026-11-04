//! Scoped runs: a worker restricted to named tasks (`fw worker --task ID`).
//!
//! A [`TaskScope`] narrows every action an [`super::orchestrate`] session
//! takes -- stranded recovery, reactive resume and approval, landing
//! re-drive, the pre-run stop ledger, and fresh selection -- to its member
//! ids, while the pass still reads the WHOLE listing so a DONE prerequisite
//! outside the scope still promotes a member. Claims are untouched: a
//! member another worker holds is skipped exactly as an unscoped worker
//! skips it, never stolen.
//!
//! [`plan_scope`] is the start-up gate: it refuses an unknown id, an
//! unrunnable task, and (unless prerequisites are pulled in) a named task
//! whose prerequisites are not done. [`evaluate_scope`] is the exit test:
//! once every named task is settled -- verified, failed after the scope
//! started, parked, or blocked behind a member that settled unverified --
//! the scoped worker stops instead of idling.

use crate::rollup::{TaskState, TaskStatusRow, status_rows_for_items, task_state};
use crate::sources::{WorkItem, WorkSource, WorkSourceError};
use crate::work_graph::WorkGraph;
use chrono::{DateTime, Utc};
use flywheel_core::events::{DomainEventKind, DomainEventPayload};
use flywheel_core::lifecycle::Status;
use flywheel_core::store::{HarnessStore, LifecycleFilter, StoreError};
use flywheel_core::validation::validate_task;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::path::Path;

/// The task ids a scoped session may act on, and the instant the scope
/// began (a member whose latest lifecycle failed at or after it was
/// attempted under this scope and is not run again).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskScope {
    ids: BTreeSet<String>,
    since: DateTime<Utc>,
}

impl TaskScope {
    pub fn new(ids: impl IntoIterator<Item = String>, since: DateTime<Utc>) -> Self {
        Self {
            ids: ids.into_iter().collect(),
            since,
        }
    }

    pub fn contains(&self, task_id: &str) -> bool {
        self.ids.contains(task_id)
    }

    pub fn ids(&self) -> &BTreeSet<String> {
        &self.ids
    }

    pub fn since(&self) -> DateTime<Utc> {
        self.since
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
}

/// `true` when `scope` is unset or holds `task_id`.
pub(crate) fn in_scope(scope: Option<&TaskScope>, task_id: &str) -> bool {
    scope.is_none_or(|scope| scope.contains(task_id))
}

/// Whether the member's latest lifecycle failed at or after the scope
/// began: it was attempted under this scope (by this worker or a peer
/// sharing it) and must not be run again.
pub(crate) fn failed_under_scope(
    control: &dyn HarnessStore,
    row: &TaskStatusRow,
    scope: &TaskScope,
) -> Result<bool, StoreError> {
    if row.state != TaskState::Retryable {
        return Ok(false);
    }
    let Some(run_id) = &row.latest_run_id else {
        return Ok(false);
    };
    let Some(lifecycle) = control.load_lifecycle(run_id)? else {
        return Ok(false);
    };
    Ok(lifecycle
        .timestamps
        .get(&lifecycle.status)
        .is_some_and(|at| *at >= scope.since))
}

/// What a scoped run was asked for.
#[derive(Debug, Clone, Default)]
pub struct ScopeRequest {
    /// The named task ids, in the operator's order (duplicates collapse).
    pub tasks: Vec<String>,
    /// Pull every not-done prerequisite (transitively) into the scope
    /// instead of refusing.
    pub with_prerequisites: bool,
    /// The worker's `[execution] capabilities`.
    pub worker_capabilities: BTreeSet<String>,
}

/// A named task's prerequisite that is not done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnmetPrerequisite {
    pub task_id: String,
    pub prerequisite: String,
    /// The prerequisite's state (`fresh`, `retryable`, ...), or
    /// `unlisted` for one no work source lists and the store never finished.
    pub state: String,
}

/// Why a scope cannot be run.
#[derive(Debug, thiserror::Error)]
pub enum ScopeError {
    #[error("no task given")]
    Empty,
    #[error("unknown task {}: the work source does not list {} and the store never finished {}", .0.join(", "), if .0.len() == 1 { "it" } else { "them" }, if .0.len() == 1 { "it" } else { "them" })]
    Unknown(Vec<String>),
    #[error("{}", render_unmet(.0))]
    UnmetPrerequisites(Vec<UnmetPrerequisite>),
    #[error("{}", render_unrunnable(.0))]
    Unrunnable(Vec<(String, String)>),
    #[error("reading the store: {0}")]
    Store(#[from] StoreError),
    #[error("listing the work source: {0}")]
    Source(#[from] WorkSourceError),
}

fn render_unmet(unmet: &[UnmetPrerequisite]) -> String {
    let mut by_task: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for u in unmet {
        by_task
            .entry(u.task_id.as_str())
            .or_default()
            .push(format!("{} ({})", u.prerequisite, u.state));
    }
    by_task
        .into_iter()
        .map(|(task, prereqs)| {
            format!(
                "task {task} waits on unmet prerequisite{} {}",
                if prereqs.len() == 1 { "" } else { "s" },
                prereqs.join(", ")
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn render_unrunnable(tasks: &[(String, String)]) -> String {
    tasks
        .iter()
        .map(|(task, why)| format!("task {task} cannot run here: {why}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// A validated scope: the named tasks plus any prerequisites pulled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopePlan {
    /// The named tasks, in the operator's order.
    pub named: Vec<String>,
    /// Prerequisites pulled in by `with_prerequisites`, in listing order.
    pub added: Vec<String>,
}

impl ScopePlan {
    /// Every member id (named plus added).
    pub fn members(&self) -> BTreeSet<String> {
        self.named.iter().chain(&self.added).cloned().collect()
    }

    /// The session scope for this plan, starting at `since`.
    pub fn scope(&self, since: DateTime<Utc>) -> TaskScope {
        TaskScope::new(self.members(), since)
    }
}

/// One listing pass's view: items, rows, and the store-done ids the
/// listing does not carry.
struct Listing {
    items: HashMap<String, WorkItem>,
    order: Vec<String>,
    rows: HashMap<String, TaskStatusRow>,
    excluded: HashMap<String, String>,
}

impl Listing {
    fn read(source: &dyn WorkSource, control: &dyn HarnessStore) -> Result<Self, ScopeError> {
        let items = source.list_work()?;
        let rows = status_rows_for_items(&items, control)?;
        let validation = WorkGraph::build(items.iter().cloned());
        Ok(Self {
            order: items.iter().map(|i| i.task.id.clone()).collect(),
            rows: rows.into_iter().map(|r| (r.task_id.clone(), r)).collect(),
            excluded: validation
                .excluded
                .into_iter()
                .map(|e| (e.task_id, e.reason))
                .collect(),
            items: items.into_iter().map(|i| (i.task.id.clone(), i)).collect(),
        })
    }

    /// `Some(state)` for a listed task; `None` for an unlisted one.
    fn state(&self, task_id: &str) -> Option<TaskState> {
        self.rows.get(task_id).map(|r| r.state)
    }
}

/// Done in the authoritative store (an archived task the listing no
/// longer carries still satisfies its dependents).
fn store_done(control: &dyn HarnessStore, task_id: &str) -> Result<bool, StoreError> {
    Ok(task_state(control, task_id)?.state == TaskState::Done)
}

/// Validate `request` against the current listing and store.
///
/// Refuses (nothing run) an empty request, an id neither listed nor done
/// in the store, a member the graph isolates (duplicate id, cycle),
/// whose capabilities the worker lacks, or whose graders are statically
/// broken, and -- without `with_prerequisites` -- a named task with a
/// prerequisite that is not done. With it, every not-done prerequisite is
/// added (transitively); one no source lists is still refused.
pub fn plan_scope(
    source: &dyn WorkSource,
    control: &dyn HarnessStore,
    request: &ScopeRequest,
    repo_root: Option<&Path>,
) -> Result<ScopePlan, ScopeError> {
    let mut named: Vec<String> = Vec::new();
    for id in &request.tasks {
        if !named.contains(id) {
            named.push(id.clone());
        }
    }
    if named.is_empty() {
        return Err(ScopeError::Empty);
    }
    let listing = Listing::read(source, control)?;
    let mut unknown = Vec::new();
    for id in &named {
        if listing.state(id).is_none() && !store_done(control, id)? {
            unknown.push(id.clone());
        }
    }
    if !unknown.is_empty() {
        return Err(ScopeError::Unknown(unknown));
    }

    let mut unmet: Vec<UnmetPrerequisite> = Vec::new();
    let mut added: BTreeSet<String> = BTreeSet::new();
    let mut members: BTreeSet<String> = named.iter().cloned().collect();
    let mut frontier: Vec<String> = named.clone();
    while let Some(id) = frontier.pop() {
        let Some(item) = listing.items.get(&id) else {
            continue; // done in the store: nothing to run, nothing below it
        };
        if listing.state(&id) == Some(TaskState::Done) {
            continue;
        }
        for prereq in &item.prerequisites {
            let state = match listing.state(prereq) {
                Some(TaskState::Done) => continue,
                Some(state) => state.as_str().to_owned(),
                None if store_done(control, prereq)? => continue,
                None => "unlisted".to_owned(),
            };
            let pullable = request.with_prerequisites && state != "unlisted";
            if pullable {
                if members.insert(prereq.clone()) {
                    added.insert(prereq.clone());
                    frontier.push(prereq.clone());
                }
            } else if !unmet
                .iter()
                .any(|u| u.task_id == id && &u.prerequisite == prereq)
            {
                unmet.push(UnmetPrerequisite {
                    task_id: id.clone(),
                    prerequisite: prereq.clone(),
                    state,
                });
            }
        }
    }
    if !unmet.is_empty() {
        return Err(ScopeError::UnmetPrerequisites(unmet));
    }

    let mut unrunnable: Vec<(String, String)> = Vec::new();
    for id in listing.order.iter().filter(|id| members.contains(*id)) {
        if listing.state(id) == Some(TaskState::Done) {
            continue;
        }
        let item = &listing.items[id];
        if let Some(reason) = listing.excluded.get(id) {
            unrunnable.push((id.clone(), reason.clone()));
            continue;
        }
        let missing: Vec<&str> = item
            .required_capabilities
            .difference(&request.worker_capabilities)
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            unrunnable.push((
                id.clone(),
                format!(
                    "it requires capabilities this worker lacks ({}; see [execution] capabilities)",
                    missing.join(", ")
                ),
            ));
            continue;
        }
        if let Some(root) = repo_root {
            let defects = validate_task(&item.task, root);
            if !defects.is_empty() {
                let details: Vec<String> = defects.into_iter().map(|d| d.detail).collect();
                unrunnable.push((
                    id.clone(),
                    format!("invalid task definition ({})", details.join("; ")),
                ));
            }
        }
    }
    if !unrunnable.is_empty() {
        return Err(ScopeError::Unrunnable(unrunnable));
    }
    Ok(ScopePlan {
        named,
        added: listing
            .order
            .iter()
            .filter(|id| added.contains(*id))
            .cloned()
            .collect(),
    })
}

/// Where one scope member stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeOutcome {
    /// Done, and its landing (if the strategy records one) not parked.
    Verified,
    /// Its latest lifecycle failed after the scope began (retries spent).
    Failed { reason: String },
    /// Done but its landing parked, awaiting approval, blocked on a
    /// predicate, or dropped from the listing: needs an operator.
    Parked { reason: String },
    /// Waits on a scope member that settled without being verified.
    Blocked { prerequisite: String },
    /// Not settled yet (not run, running, or interrupted and resumable).
    Pending,
}

impl ScopeOutcome {
    pub fn is_settled(&self) -> bool {
        !matches!(self, ScopeOutcome::Pending)
    }

    pub fn label(&self) -> &'static str {
        match self {
            ScopeOutcome::Verified => "verified",
            ScopeOutcome::Failed { .. } => "failed",
            ScopeOutcome::Parked { .. } => "parked",
            ScopeOutcome::Blocked { .. } => "blocked",
            ScopeOutcome::Pending => "pending",
        }
    }
}

impl fmt::Display for ScopeOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScopeOutcome::Verified | ScopeOutcome::Pending => f.write_str(self.label()),
            ScopeOutcome::Failed { reason } | ScopeOutcome::Parked { reason } => {
                if reason.is_empty() {
                    f.write_str(self.label())
                } else {
                    write!(f, "{} ({reason})", self.label())
                }
            }
            ScopeOutcome::Blocked { prerequisite } => {
                write!(f, "blocked (prerequisite {prerequisite} did not verify)")
            }
        }
    }
}

/// Every named task's outcome, in the plan's order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeVerdict {
    pub outcomes: Vec<(String, ScopeOutcome)>,
}

impl ScopeVerdict {
    /// Every named task settled: the scoped worker may stop.
    pub fn settled(&self) -> bool {
        self.outcomes.iter().all(|(_, o)| o.is_settled())
    }

    pub fn all_verified(&self) -> bool {
        self.outcomes
            .iter()
            .all(|(_, o)| *o == ScopeOutcome::Verified)
    }

    /// The one-line summary: `scope: 1/2 verified -- a verified; b failed (...)`.
    pub fn summary(&self) -> String {
        let verified = self
            .outcomes
            .iter()
            .filter(|(_, o)| *o == ScopeOutcome::Verified)
            .count();
        let parts: Vec<String> = self
            .outcomes
            .iter()
            .map(|(id, outcome)| format!("{id} {}", one_line(&outcome.to_string())))
            .collect();
        format!(
            "scope: {verified}/{} verified -- {}",
            self.outcomes.len(),
            parts.join("; ")
        )
    }
}

/// Collapse whitespace runs (an error message's newlines) and cap the
/// length so the summary stays one readable line.
fn one_line(text: &str) -> String {
    const LIMIT: usize = 160;
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= LIMIT {
        return collapsed;
    }
    let cut: String = collapsed.chars().take(LIMIT - 3).collect();
    format!("{cut}...")
}

/// The landing verdict of a DONE task's latest DONE run: parked when its
/// most recent landing record is a park (no later `Landed`).
fn landing_park(control: &dyn HarnessStore, task_id: &str) -> Result<Option<String>, StoreError> {
    let filter = LifecycleFilter::default()
        .with_task_id(task_id)
        .with_statuses([Status::Done]);
    let Some(done) = control.list_lifecycles(&filter)?.into_iter().next() else {
        return Ok(None);
    };
    let Some(park) =
        control.latest_domain_event_of_kind(&done.run_id, DomainEventKind::LandingParked)?
    else {
        return Ok(None);
    };
    let landed_after = control
        .latest_domain_event_of_kind(&done.run_id, DomainEventKind::Landed)?
        .is_some_and(|landed| landed.ts >= park.ts);
    if landed_after {
        return Ok(None);
    }
    Ok(match &park.payload {
        DomainEventPayload::LandingParked(parked) if parked.detail.is_empty() => {
            Some(format!("landing parked: {}", parked.park_kind))
        }
        DomainEventPayload::LandingParked(parked) => Some(format!(
            "landing parked: {} -- {}",
            parked.park_kind, parked.detail
        )),
        _ => Some("landing parked".to_owned()),
    })
}

fn own_outcome(
    control: &dyn HarnessStore,
    listing: &Listing,
    scope: &TaskScope,
    task_id: &str,
) -> Result<ScopeOutcome, StoreError> {
    let Some(row) = listing.rows.get(task_id) else {
        return Ok(if store_done(control, task_id)? {
            ScopeOutcome::Verified
        } else {
            ScopeOutcome::Parked {
                reason: "no longer listed by the work source".to_owned(),
            }
        });
    };
    Ok(match row.state {
        TaskState::Done => match landing_park(control, task_id)? {
            Some(reason) => ScopeOutcome::Parked { reason },
            None => ScopeOutcome::Verified,
        },
        TaskState::AwaitingApproval => ScopeOutcome::Parked {
            reason: "awaiting approval (fw approve / fw reject)".to_owned(),
        },
        TaskState::Interrupted if row.blocked_requires.is_some() => ScopeOutcome::Parked {
            reason: "blocked on its requires predicates".to_owned(),
        },
        TaskState::Retryable if failed_under_scope(control, row, scope)? => ScopeOutcome::Failed {
            reason: row.latest_error.clone(),
        },
        _ => ScopeOutcome::Pending,
    })
}

/// Where every named task of `plan` stands under `scope`.
///
/// A named task still pending whose in-scope prerequisite (transitively)
/// settled without verifying can never become runnable: it is reported
/// `Blocked` on that prerequisite, so the scope settles instead of idling.
pub fn evaluate_scope(
    source: &dyn WorkSource,
    control: &dyn HarnessStore,
    plan: &ScopePlan,
    scope: &TaskScope,
) -> Result<ScopeVerdict, ScopeError> {
    let listing = Listing::read(source, control)?;
    let mut own: HashMap<String, ScopeOutcome> = HashMap::new();
    for id in scope.ids() {
        own.insert(id.clone(), own_outcome(control, &listing, scope, id)?);
    }
    let blocker = |start: &str| -> Option<String> {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut stack: Vec<String> = vec![start.to_owned()];
        while let Some(id) = stack.pop() {
            let Some(item) = listing.items.get(&id) else {
                continue;
            };
            for prereq in &item.prerequisites {
                if !scope.contains(prereq) || !seen.insert(prereq.clone()) {
                    continue;
                }
                match own.get(prereq) {
                    Some(ScopeOutcome::Verified) => {}
                    Some(ScopeOutcome::Pending) | None => stack.push(prereq.clone()),
                    Some(_) => return Some(prereq.clone()),
                }
            }
        }
        None
    };
    let outcomes = plan
        .named
        .iter()
        .map(|id| {
            let outcome = match own.get(id).cloned().unwrap_or(ScopeOutcome::Pending) {
                ScopeOutcome::Pending => match blocker(id) {
                    Some(prerequisite) => ScopeOutcome::Blocked { prerequisite },
                    None => ScopeOutcome::Pending,
                },
                settled => settled,
            };
            (id.clone(), outcome)
        })
        .collect();
    Ok(ScopeVerdict { outcomes })
}
