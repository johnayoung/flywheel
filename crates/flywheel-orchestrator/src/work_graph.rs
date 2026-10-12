//! WorkGraph: an explicit, validated task DAG over [`WorkItem`] values.
//!
//! The orchestrator's prerequisite edges ride along on each `WorkItem`
//! (`WorkItem.prerequisites`). This module lifts those implicit edges into
//! a first-class, validated object so structural corruption is caught
//! once, loudly, *before* any scheduling pass, instead of silently
//! producing a graph that deadlocks or schedules a task that can never
//! satisfy its own precondition.
//!
//! Every defect is *contained, never fatal* (spec 00065 extends the
//! missing-prerequisite posture of spec 00047 to structural corruption) so
//! one corrupt definition never poisons the unrelated tasks sharing its
//! scheduling pass:
//!
//! * **Structural corruption** (a duplicate task id, a self-dependency, a
//!   cycle) isolates the offending task(s). Construction records an
//!   [`ExcludedTask`] whose `reason` names the offending id(s) (for a
//!   cycle, every participating member) and builds the graph from the
//!   survivors. The isolated ids never reach [`WorkGraph::items`], so they
//!   stay out of [`WorkGraph::ready_set`] while every valid, independent
//!   task in the same pass remains schedulable.
//! * **A missing prerequisite** (an edge that resolves to no node). Under
//!   multi-source aggregation the referenced work may simply not be loaded
//!   by a sibling source on this pass, so construction records a
//!   [`GraphValidationIssue`] naming the referencing and missing ids, and
//!   the referencing task stays out of the ready set. A survivor whose
//!   prerequisite was *isolated* above keeps a dangling edge of exactly
//!   this class.
//!
//! Callers that want the hard-fail posture (`docs/orchestration.md`'s
//! `WorkGraphValidationError`) use [`WorkGraph::build_strict`], which turns
//! any exclusion into that error.
//!
//! Authoritative spec: `docs/orchestration.md` (The WorkGraph).

use crate::sources::{WorkItem, WorkSource, WorkSourceError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt;

/// The own-state values that make a task eligible for the ready set:
/// mirrors the scheduler's `TaskState` (`fresh`, `retryable`,
/// `interrupted`) without importing it, so this graph-model module stays
/// decoupled from the scheduler. Callers pass state values as strings (or
/// any `AsRef<str>`, which a `TaskState` enum implements).
pub const ELIGIBLE_STATE_VALUES: [&str; 3] = ["fresh", "retryable", "interrupted"];

/// The own-state value that satisfies a dependent's prerequisite edge.
pub const DONE_STATE_VALUE: &str = "done";

/// A task isolated from a [`WorkGraph`] for structural corruption.
///
/// Recorded (never raised) when construction encounters a duplicate task
/// id, a self-dependency, or a cycle: `task_id` is the isolated task and
/// `reason` is a human-readable explanation that always names the
/// offending id(s); for a cycle, every participating member. The isolated
/// task is absent from [`WorkGraph::items`] and the ready set, while every
/// valid, independent task in the same pass stays schedulable.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExcludedTask {
    pub task_id: String,
    pub reason: String,
}

/// A non-fatal defect surfaced while building a [`WorkGraph`].
///
/// The only such class today is a dangling prerequisite: `referencing_id`
/// is the task that declared the edge, `missing_id` the prerequisite id
/// that resolves to no item in the graph. The issue is recorded (never
/// raised) and the referencing task stays out of [`WorkGraph::ready_set`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GraphValidationIssue {
    pub referencing_id: String,
    pub missing_id: String,
}

impl GraphValidationIssue {
    /// Alias for `referencing_id` (the edge's owner).
    pub fn task_id(&self) -> &str {
        &self.referencing_id
    }

    /// Alias for `missing_id` (the unresolved prerequisite).
    pub fn missing_prerequisite(&self) -> &str {
        &self.missing_id
    }
}

impl fmt::Display for GraphValidationIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "task {:?} declares missing prerequisite {:?}",
            self.referencing_id, self.missing_id
        )
    }
}

/// A built [`WorkGraph`] together with its recorded defects.
///
/// Returned by [`WorkGraph::build`]. `graph` is the validated graph built
/// from the survivors; `issues` holds the recorded missing-prerequisite
/// findings (empty when every edge resolved); `excluded` holds the tasks
/// isolated for structural corruption, each with a reason naming the
/// offender (empty when the set was structurally sound).
#[derive(Debug, Clone, PartialEq)]
pub struct GraphValidationResult {
    pub graph: WorkGraph,
    pub issues: Vec<GraphValidationIssue>,
    pub excluded: Vec<ExcludedTask>,
}

/// The hard-fail form of structural corruption, for callers that would
/// rather abort a pass than schedule around an isolated task. Carries
/// every exclusion the contained build recorded; the message names each
/// offender.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct WorkGraphValidationError {
    pub excluded: Vec<ExcludedTask>,
}

impl fmt::Display for WorkGraphValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut reasons: Vec<&str> = Vec::new();
        for exclusion in &self.excluded {
            if !reasons.contains(&exclusion.reason.as_str()) {
                reasons.push(&exclusion.reason);
            }
        }
        write!(
            f,
            "work graph is structurally invalid ({} task(s) isolated): {}",
            self.excluded.len(),
            reasons.join("; ")
        )
    }
}

/// The caller-side inputs to [`WorkGraph::ready_set`] beyond the state map.
///
/// `excluded` drops ids from candidacy for this pass without treating them
/// as undone (an excluded prerequisite still satisfies its dependents).
/// `worker_capabilities` is the worker's advertised capability set: an
/// item is offered only when its `required_capabilities` is a subset, and
/// an empty requirement matches any worker. `satisfied_prerequisites` is
/// the set of prerequisite ids the store's authoritative lifecycle record
/// already resolves to DONE even though no listed node provides them (a
/// prerequisite whose defining task's phase archived); the graph stays
/// store-agnostic and takes the result as data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadySetQuery {
    pub excluded: HashSet<String>,
    pub worker_capabilities: BTreeSet<String>,
    pub satisfied_prerequisites: HashSet<String>,
}

impl ReadySetQuery {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_excluded<I, S>(mut self, ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.excluded = ids.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_worker_capabilities<I, S>(mut self, capabilities: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.worker_capabilities = capabilities.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_satisfied_prerequisites<I, S>(mut self, ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.satisfied_prerequisites = ids.into_iter().map(Into::into).collect();
        self
    }
}

/// A validated prerequisite DAG built from a sequence of [`WorkItem`]s.
///
/// Construction validates structure eagerly but *contains* every defect
/// rather than failing. A duplicate task id, a self-dependency, or a cycle
/// isolates the offending task(s): the graph is built from the survivors
/// and each exclusion is recorded as an [`ExcludedTask`] with a reason
/// naming the offender(s). Missing prerequisites are recorded as
/// [`GraphValidationIssue`]s. Either way, every valid, independent task in
/// the same pass stays schedulable.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkGraph {
    items: Vec<WorkItem>,
    index: HashMap<String, usize>,
    resolved: Vec<Vec<String>>,
    issues: Vec<GraphValidationIssue>,
    excluded: Vec<ExcludedTask>,
}

impl Default for WorkGraph {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl WorkGraph {
    /// Build the graph, isolating structurally corrupt tasks and recording
    /// missing prerequisites. Never fails.
    pub fn new(items: impl IntoIterator<Item = WorkItem>) -> Self {
        let materialized: Vec<WorkItem> = items.into_iter().collect();
        let mut excluded: Vec<ExcludedTask> = Vec::new();
        let mut excluded_ids: HashSet<String> = HashSet::new();

        // -- duplicate task id (structural corruption) --------------------
        // An id defined more than once is ambiguous (nothing can say which
        // definition is authoritative), so every copy is isolated and the
        // id is named once. First-seen order keeps the record deterministic.
        let mut counts: HashMap<&str, usize> = HashMap::new();
        let mut first_seen: Vec<&str> = Vec::new();
        for item in &materialized {
            let count = counts.entry(item.id()).or_insert(0);
            if *count == 0 {
                first_seen.push(item.id());
            }
            *count += 1;
        }
        for task_id in first_seen {
            let count = counts[task_id];
            if count > 1 {
                excluded_ids.insert(task_id.to_string());
                excluded.push(ExcludedTask {
                    task_id: task_id.to_string(),
                    reason: format!(
                        "duplicate task id: {task_id:?} is defined {count} times in the same scheduling pass"
                    ),
                });
            }
        }

        // -- self-dependency (structural corruption) ----------------------
        // A task that lists itself as a prerequisite can never satisfy its
        // own precondition; isolate it. A copy already isolated as a
        // duplicate is not recorded twice.
        for item in &materialized {
            let task_id = item.id();
            if excluded_ids.contains(task_id) {
                continue;
            }
            if item.prerequisites.iter().any(|p| p == task_id) {
                excluded_ids.insert(task_id.to_string());
                excluded.push(ExcludedTask {
                    task_id: task_id.to_string(),
                    reason: format!(
                        "self-dependency: task {task_id:?} lists itself as a prerequisite"
                    ),
                });
            }
        }

        // -- cycle detection (structural corruption) ----------------------
        // Detect cycles over the survivors' resolved edges, then isolate
        // every participating member, naming the whole cycle in each
        // member's reason so the corrupt loop is recoverable from any id.
        let survivor_ids: HashSet<&str> = materialized
            .iter()
            .map(WorkItem::id)
            .filter(|id| !excluded_ids.contains(*id))
            .collect();
        let resolved_for_cycle: Vec<(&str, Vec<&str>)> = materialized
            .iter()
            .filter(|item| !excluded_ids.contains(item.id()))
            .map(|item| {
                let edges = item
                    .prerequisites
                    .iter()
                    .map(String::as_str)
                    .filter(|p| survivor_ids.contains(p))
                    .collect();
                (item.id(), edges)
            })
            .collect();
        let cycle_members = cycle_members(&resolved_for_cycle);
        if !cycle_members.is_empty() {
            let named = cycle_members
                .iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            for member in &cycle_members {
                excluded_ids.insert(member.to_string());
                excluded.push(ExcludedTask {
                    task_id: member.to_string(),
                    reason: format!("dependency cycle among task id(s): {named}"),
                });
            }
        }

        // -- build the graph from the survivors ---------------------------
        // Each item's declared prerequisites stay as authored (used to keep
        // a task with a dangling edge ineligible); `resolved` keeps only the
        // edges that point at a surviving node (used for topological order
        // and the resolved-prerequisite query). A survivor whose
        // prerequisite was isolated above keeps a dangling edge, recorded
        // as a GraphValidationIssue exactly like an unresolved cross-source
        // reference, so it stays out of the ready set.
        let items: Vec<WorkItem> = materialized
            .into_iter()
            .filter(|item| !excluded_ids.contains(item.id()))
            .collect();
        let index: HashMap<String, usize> = items
            .iter()
            .enumerate()
            .map(|(position, item)| (item.id().to_string(), position))
            .collect();
        let mut resolved = Vec::with_capacity(items.len());
        let mut issues = Vec::new();
        for item in &items {
            let mut edges = Vec::new();
            for prereq_id in &item.prerequisites {
                if index.contains_key(prereq_id) {
                    edges.push(prereq_id.clone());
                } else {
                    issues.push(GraphValidationIssue {
                        referencing_id: item.id().to_string(),
                        missing_id: prereq_id.clone(),
                    });
                }
            }
            resolved.push(edges);
        }

        Self {
            items,
            index,
            resolved,
            issues,
            excluded,
        }
    }

    // -- builders ------------------------------------------------------------

    /// Construct the graph and return it with its recorded defects.
    ///
    /// Never fails: structural corruption isolates the offending task(s)
    /// and missing prerequisites are recorded.
    pub fn build(items: impl IntoIterator<Item = WorkItem>) -> GraphValidationResult {
        Self::new(items).into_validation()
    }

    /// Construct the graph, failing on any structural corruption.
    ///
    /// Missing prerequisites are still recorded on the returned graph's
    /// `issues`; only duplicate ids, self-dependencies, and cycles are
    /// fatal here.
    pub fn build_strict(
        items: impl IntoIterator<Item = WorkItem>,
    ) -> Result<WorkGraph, WorkGraphValidationError> {
        let graph = Self::new(items);
        if graph.excluded.is_empty() {
            Ok(graph)
        } else {
            Err(WorkGraphValidationError {
                excluded: graph.excluded,
            })
        }
    }

    /// This graph paired with its recorded issues and exclusions.
    pub fn into_validation(self) -> GraphValidationResult {
        let issues = self.issues.clone();
        let excluded = self.excluded.clone();
        GraphValidationResult {
            graph: self,
            issues,
            excluded,
        }
    }

    // -- structure queries ---------------------------------------------------

    /// The graph's items, in construction (walk) order.
    pub fn items(&self) -> &[WorkItem] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn contains(&self, task_id: &str) -> bool {
        self.index.contains_key(task_id)
    }

    pub fn get(&self, task_id: &str) -> Option<&WorkItem> {
        self.index
            .get(task_id)
            .map(|&position| &self.items[position])
    }

    /// Recorded non-fatal issues (missing prerequisites).
    pub fn issues(&self) -> &[GraphValidationIssue] {
        &self.issues
    }

    /// Tasks isolated for structural corruption, each with a reason.
    ///
    /// One [`ExcludedTask`] per task isolated for a duplicate id, a
    /// self-dependency, or a cycle. Empty when the input set was
    /// structurally sound. The excluded ids are absent from `items` and
    /// the ready set.
    pub fn excluded(&self) -> &[ExcludedTask] {
        &self.excluded
    }

    /// Prerequisite ids of `task_id` exactly as authored, dangling edges
    /// included. Empty for an unknown task.
    pub fn declared_prerequisites(&self, task_id: &str) -> &[String] {
        self.get(task_id)
            .map(|item| item.prerequisites.as_slice())
            .unwrap_or(&[])
    }

    /// Prerequisite ids of `task_id` that resolve to a real node.
    ///
    /// Dangling edges are excluded (they appear in `issues`). Empty for a
    /// prerequisite-free or unknown task.
    pub fn resolved_prerequisites(&self, task_id: &str) -> BTreeSet<String> {
        self.index
            .get(task_id)
            .map(|&position| self.resolved[position].iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Alias for [`WorkGraph::resolved_prerequisites`].
    pub fn prerequisites_of(&self, task_id: &str) -> BTreeSet<String> {
        self.resolved_prerequisites(task_id)
    }

    /// Resolved prerequisite edges as `{task_id: {prereq_id, ...}}`.
    pub fn edges(&self) -> BTreeMap<String, BTreeSet<String>> {
        self.items
            .iter()
            .zip(&self.resolved)
            .map(|(item, edges)| (item.id().to_string(), edges.iter().cloned().collect()))
            .collect()
    }

    /// Task ids in a topological order (prerequisites before dependents).
    ///
    /// Defined over the resolved edges only; ties break by construction
    /// order (Kahn's algorithm over a FIFO frontier). The graph is acyclic
    /// here (every cycle member was isolated), so a total order always
    /// exists.
    pub fn topological_order(&self) -> Vec<String> {
        let n = self.items.len();
        let mut indegree: Vec<usize> = self.resolved.iter().map(Vec::len).collect();
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (position, edges) in self.resolved.iter().enumerate() {
            for prereq_id in edges {
                dependents[self.index[prereq_id]].push(position);
            }
        }
        let mut ready: VecDeque<usize> = (0..n).filter(|&i| indegree[i] == 0).collect();
        let mut out = Vec::with_capacity(n);
        while let Some(position) = ready.pop_front() {
            out.push(self.items[position].id().to_string());
            for &dependent in &dependents[position] {
                indegree[dependent] -= 1;
                if indegree[dependent] == 0 {
                    ready.push_back(dependent);
                }
            }
        }
        out
    }

    // -- scheduling query ----------------------------------------------------

    /// Every currently-runnable item, and only those.
    ///
    /// An item is runnable when:
    ///
    /// * its id is not in `query.excluded`, AND
    /// * its own state (`states[id]`) is eligible: `fresh`, `retryable`,
    ///   or `interrupted`, AND
    /// * every *declared* prerequisite either resolves to a node whose
    ///   state is `done` or is listed in `query.satisfied_prerequisites`,
    ///   AND
    /// * its `required_capabilities` is a subset of
    ///   `query.worker_capabilities`.
    ///
    /// `states` maps task id to its state value; a task with a dangling
    /// prerequisite, an unknown/ineligible own state, or an excluded id is
    /// omitted. Returns *all* matches (no early return) so parallel
    /// children all surface.
    ///
    /// The returned items are ordered by descending `priority`, ties broken
    /// by construction (walk) order via a stable sort (spec 00049, D-1), so
    /// the caller's first element is the highest-priority runnable item and
    /// an all-default (priority 0) set keeps pure walk order.
    pub fn ready_set<S: AsRef<str>>(
        &self,
        states: &HashMap<String, S>,
        query: &ReadySetQuery,
    ) -> Vec<&WorkItem> {
        let state_of = |task_id: &str| states.get(task_id).map(AsRef::as_ref);
        let mut runnable: Vec<&WorkItem> = self
            .items
            .iter()
            .filter(|item| !query.excluded.contains(item.id()))
            .filter(|item| state_of(item.id()).is_some_and(|s| ELIGIBLE_STATE_VALUES.contains(&s)))
            .filter(|item| {
                item.required_capabilities
                    .is_subset(&query.worker_capabilities)
            })
            .filter(|item| {
                item.prerequisites.iter().all(|prereq_id| {
                    (self.index.contains_key(prereq_id)
                        && state_of(prereq_id) == Some(DONE_STATE_VALUE))
                        || query.satisfied_prerequisites.contains(prereq_id)
                })
            })
            .collect();
        // Stable descending-priority sort: equal-priority items keep walk
        // order, so an all-default set is byte-identical to construction
        // order.
        runnable.sort_by_key(|item| std::cmp::Reverse(item.priority));
        runnable
    }
}

/// Aggregates `list_work()` across sources into one validated graph.
///
/// The builder is the multi-source seam: it asks each [`WorkSource`] for
/// its current items, concatenates them into a single combined set, and
/// only THEN hands that set to [`WorkGraph::build`]. Validation over the
/// aggregate, never per source, is the load-bearing property (spec 00047,
/// D-1): a prerequisite declared by an item from one source and satisfied
/// by an item from another resolves as a real edge, while a reference
/// unresolved in *every* source becomes a non-fatal
/// [`GraphValidationIssue`] rather than aborting the build. Structural
/// corruption in the combined set isolates the offending task(s) as
/// [`ExcludedTask`] records, exactly as constructing the model directly
/// would; the surviving items still build a graph.
///
/// A single source is just the degenerate aggregation: its graph is
/// identical in edges and issues to building the model straight from that
/// source's items.
#[derive(Debug, Clone, Copy, Default)]
pub struct WorkGraphBuilder;

impl WorkGraphBuilder {
    /// Aggregate every source's items, then build and validate the graph.
    ///
    /// Each source's `list_work` is called once, in argument order, and the
    /// items are concatenated preserving that order so selection ties still
    /// break deterministically. A listing failure aborts the build with
    /// that source's error: a raising source is never read as "no work".
    pub fn build<'a>(
        sources: impl IntoIterator<Item = &'a dyn WorkSource>,
    ) -> Result<GraphValidationResult, WorkSourceError> {
        let mut items: Vec<WorkItem> = Vec::new();
        for source in sources {
            items.extend(source.list_work()?);
        }
        Ok(WorkGraph::build(items))
    }

    /// [`WorkGraphBuilder::build`] over owned adapters.
    pub fn build_boxed(
        sources: &[Box<dyn WorkSource>],
    ) -> Result<GraphValidationResult, WorkSourceError> {
        Self::build(sources.iter().map(Box::as_ref))
    }
}

/// Every node id that participates in a directed cycle.
///
/// Edges point from a task to each of its prerequisites. Members are the
/// union of all strongly-connected components of size > 1 (a 3-node cycle
/// a->b->c->a yields `{a, b, c}`). Self-loops are handled earlier as
/// self-dependencies, so only multi-node cycles reach here. Tarjan's SCC
/// algorithm, iterative to avoid recursion limits on long chains; the
/// result is sorted so reasons read deterministically.
fn cycle_members<'a>(resolved: &[(&'a str, Vec<&'a str>)]) -> Vec<&'a str> {
    let n = resolved.len();
    let position: HashMap<&str, usize> = resolved
        .iter()
        .enumerate()
        .map(|(i, (id, _))| (*id, i))
        .collect();
    let successors: Vec<Vec<usize>> = resolved
        .iter()
        .map(|(_, edges)| edges.iter().map(|e| position[e]).collect())
        .collect();

    const UNVISITED: usize = usize::MAX;
    let mut index = vec![UNVISITED; n];
    let mut lowlink = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut next_index = 0usize;
    let mut members: Vec<&str> = Vec::new();

    for root in 0..n {
        if index[root] != UNVISITED {
            continue;
        }
        // Iterative DFS: each work frame is (node, next successor slot).
        let mut work: Vec<(usize, usize)> = vec![(root, 0)];
        index[root] = next_index;
        lowlink[root] = next_index;
        next_index += 1;
        stack.push(root);
        on_stack[root] = true;
        while let Some(&(node, mut cursor)) = work.last() {
            let frame = work.len() - 1;
            let mut advanced = false;
            while cursor < successors[node].len() {
                let successor = successors[node][cursor];
                cursor += 1;
                if index[successor] == UNVISITED {
                    index[successor] = next_index;
                    lowlink[successor] = next_index;
                    next_index += 1;
                    stack.push(successor);
                    on_stack[successor] = true;
                    work[frame].1 = cursor;
                    work.push((successor, 0));
                    advanced = true;
                    break;
                }
                if on_stack[successor] {
                    lowlink[node] = lowlink[node].min(index[successor]);
                }
            }
            if advanced {
                continue;
            }
            // All successors explored: close this node's SCC if it is a root.
            if lowlink[node] == index[node] {
                let mut component = Vec::new();
                loop {
                    let member = stack.pop().expect("Tarjan stack holds the SCC root");
                    on_stack[member] = false;
                    component.push(member);
                    if member == node {
                        break;
                    }
                }
                if component.len() > 1 {
                    members.extend(component.into_iter().map(|i| resolved[i].0));
                }
            }
            work.pop();
            if let Some(&(parent, _)) = work.last() {
                lowlink[parent] = lowlink[parent].min(lowlink[node]);
            }
        }
    }

    members.sort_unstable();
    members.dedup();
    members
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::WorkReport;
    use flywheel_core::task::{CommandGrader, Grader, Task};

    fn item(task_id: &str, prerequisites: &[&str]) -> WorkItem {
        let task = Task::new(
            format!("Goal for {task_id}."),
            vec![Grader::Command(CommandGrader::new("true").unwrap())],
        )
        .with_id(task_id);
        WorkItem::new(task, format!("/tasks/{task_id}.json"))
            .with_prerequisites(prerequisites.iter().copied())
    }

    fn fresh_states(graph: &WorkGraph) -> HashMap<String, String> {
        graph
            .items()
            .iter()
            .map(|i| (i.id().to_string(), "fresh".to_string()))
            .collect()
    }

    fn state_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(id, state)| (id.to_string(), state.to_string()))
            .collect()
    }

    fn selectable_ids(graph: &WorkGraph) -> BTreeSet<String> {
        graph
            .ready_set(&fresh_states(graph), &ReadySetQuery::default())
            .iter()
            .map(|i| i.id().to_string())
            .collect()
    }

    fn ready_ids<S: AsRef<str>>(
        graph: &WorkGraph,
        states: &HashMap<String, S>,
        query: &ReadySetQuery,
    ) -> Vec<String> {
        graph
            .ready_set(states, query)
            .iter()
            .map(|i| i.id().to_string())
            .collect()
    }

    fn item_ids(graph: &WorkGraph) -> BTreeSet<String> {
        graph.items().iter().map(|i| i.id().to_string()).collect()
    }

    fn excluded_by_id(result: &GraphValidationResult) -> HashMap<String, ExcludedTask> {
        result
            .excluded
            .iter()
            .map(|e| (e.task_id.clone(), e.clone()))
            .collect()
    }

    fn set(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    // -- duplicate id ---------------------------------------------------------

    #[test]
    fn duplicate_id_is_isolated_and_valid_peer_stays_selectable() {
        let result = WorkGraph::build([item("dup", &[]), item("dup", &[]), item("solo", &[])]);
        let graph = &result.graph;

        assert!(selectable_ids(graph).contains("solo"));
        assert!(item_ids(graph).contains("solo"));

        let excluded = excluded_by_id(&result);
        assert!(excluded["dup"].reason.contains("dup"));
        assert_eq!(
            excluded["dup"].reason,
            "duplicate task id: \"dup\" is defined 2 times in the same scheduling pass"
        );
        assert_eq!(result.excluded.len(), 1);
        assert!(!item_ids(graph).contains("dup"));
        assert!(!selectable_ids(graph).contains("dup"));
    }

    // -- self-dependency ------------------------------------------------------

    #[test]
    fn self_dependency_is_isolated_and_valid_peer_stays_selectable() {
        let result = WorkGraph::build([item("selfdep", &["selfdep"]), item("solo", &[])]);
        let graph = &result.graph;

        assert!(selectable_ids(graph).contains("solo"));
        let excluded = excluded_by_id(&result);
        assert_eq!(
            excluded["selfdep"].reason,
            "self-dependency: task \"selfdep\" lists itself as a prerequisite"
        );
        assert!(!item_ids(graph).contains("selfdep"));
        assert!(!selectable_ids(graph).contains("selfdep"));
    }

    // -- cycle ----------------------------------------------------------------

    #[test]
    fn cycle_members_are_isolated_and_valid_peer_stays_selectable() {
        let result = WorkGraph::build([
            item("alpha", &["beta"]),
            item("beta", &["alpha"]),
            item("solo", &[]),
        ]);
        let graph = &result.graph;

        assert!(selectable_ids(graph).contains("solo"));
        let excluded = excluded_by_id(&result);
        assert!(excluded.contains_key("alpha") && excluded.contains_key("beta"));
        for member in ["alpha", "beta"] {
            assert_eq!(
                excluded[member].reason,
                "dependency cycle among task id(s): alpha, beta"
            );
        }
        assert!(item_ids(graph).is_disjoint(&set(&["alpha", "beta"])));
        assert!(selectable_ids(graph).is_disjoint(&set(&["alpha", "beta"])));
    }

    #[test]
    fn three_node_cycle_names_every_member_and_spares_the_tail() {
        // a -> b -> c -> a, with d depending on a (dangling after isolation)
        // and e independent.
        let result = WorkGraph::build([
            item("a", &["b"]),
            item("b", &["c"]),
            item("c", &["a"]),
            item("d", &["a"]),
            item("e", &[]),
        ]);
        let excluded = excluded_by_id(&result);
        assert_eq!(
            excluded.keys().cloned().collect::<BTreeSet<_>>(),
            set(&["a", "b", "c"])
        );
        assert_eq!(item_ids(&result.graph), set(&["d", "e"]));
        assert_eq!(selectable_ids(&result.graph), set(&["e"]));
        assert_eq!(
            result.issues,
            vec![GraphValidationIssue {
                referencing_id: "d".into(),
                missing_id: "a".into()
            }]
        );
    }

    #[test]
    fn long_chain_does_not_overflow_and_is_acyclic() {
        let n = 20_000;
        let mut items = Vec::with_capacity(n);
        items.push(item("t0", &[]));
        for i in 1..n {
            let prev = format!("t{}", i - 1);
            items.push(item(&format!("t{i}"), &[prev.as_str()]));
        }
        let result = WorkGraph::build(items);
        assert!(result.excluded.is_empty() && result.issues.is_empty());
        assert_eq!(result.graph.len(), n);
        assert_eq!(result.graph.topological_order().len(), n);

        // Close a 1,000-node prefix into one cycle: every member is named in
        // every member's reason (quadratic in cycle size by design, so the
        // cycle is kept well below the chain length).
        let k = 1_000;
        let mut items: Vec<WorkItem> = result.graph.items()[..k].to_vec();
        items[0].prerequisites = vec![format!("t{}", k - 1)];
        let cyclic = WorkGraph::build(items);
        assert_eq!(cyclic.excluded.len(), k);
        assert!(cyclic.graph.is_empty());
        assert!(cyclic.excluded[0].reason.contains("t999"));
    }

    // -- a sound pass records nothing -----------------------------------------

    #[test]
    fn structurally_sound_pass_excludes_nothing() {
        let result = WorkGraph::build([
            item("base", &[]),
            item("leaf", &["base"]),
            item("solo", &[]),
        ]);
        assert!(result.excluded.is_empty());
        assert!(result.issues.is_empty());
        assert_eq!(item_ids(&result.graph), set(&["base", "leaf", "solo"]));
    }

    #[test]
    fn excluded_is_recorded_on_graph_and_result_alike() {
        let graph = WorkGraph::new([item("dup", &[]), item("dup", &[]), item("solo", &[])]);
        assert_eq!(
            graph
                .excluded()
                .iter()
                .map(|e| e.task_id.as_str())
                .collect::<Vec<_>>(),
            ["dup"]
        );
        let excluded = graph.excluded().to_vec();
        let result = graph.into_validation();
        assert_eq!(result.excluded, excluded);
        assert_eq!(result.graph.excluded(), excluded.as_slice());
    }

    #[test]
    fn all_three_defects_isolated_in_a_single_pass() {
        let result = WorkGraph::build([
            item("dup", &[]),
            item("dup", &[]),
            item("selfdep", &["selfdep"]),
            item("alpha", &["beta"]),
            item("beta", &["alpha"]),
            item("solo", &[]),
        ]);
        assert_eq!(selectable_ids(&result.graph), set(&["solo"]));
        let excluded = excluded_by_id(&result);
        assert_eq!(
            excluded.keys().cloned().collect::<BTreeSet<_>>(),
            set(&["dup", "selfdep", "alpha", "beta"])
        );
        for (task_id, exclusion) in &excluded {
            assert!(exclusion.reason.contains(task_id.as_str()));
        }
        // Record order is deterministic: duplicates, self-deps, then the
        // cycle members in sorted order.
        assert_eq!(
            result
                .excluded
                .iter()
                .map(|e| e.task_id.as_str())
                .collect::<Vec<_>>(),
            ["dup", "selfdep", "alpha", "beta"]
        );
    }

    #[test]
    fn survivor_depending_on_isolated_task_is_not_selectable() {
        let result = WorkGraph::build([
            item("selfdep", &["selfdep"]),
            item("dependent", &["selfdep"]),
            item("solo", &[]),
        ]);
        let graph = &result.graph;
        assert!(excluded_by_id(&result).contains_key("selfdep"));
        assert!(item_ids(graph).contains("dependent"));
        assert!(!selectable_ids(graph).contains("dependent"));
        assert!(
            result
                .issues
                .iter()
                .any(|i| i.task_id() == "dependent" && i.missing_prerequisite() == "selfdep")
        );
        assert!(selectable_ids(graph).contains("solo"));
        assert_eq!(graph.declared_prerequisites("dependent"), ["selfdep"]);
        assert!(graph.resolved_prerequisites("dependent").is_empty());
    }

    // -- strict build ---------------------------------------------------------

    #[test]
    fn build_strict_fails_on_structural_corruption_only() {
        let err = WorkGraph::build_strict([item("a", &["b"]), item("b", &["a"]), item("c", &[])])
            .unwrap_err();
        assert_eq!(err.excluded.len(), 2);
        assert_eq!(
            err.to_string(),
            "work graph is structurally invalid (2 task(s) isolated): dependency cycle among task id(s): a, b"
        );
        // A dangling edge alone is not fatal in strict mode either.
        let graph = WorkGraph::build_strict([item("b", &["missing"])]).unwrap();
        assert_eq!(graph.issues().len(), 1);
        assert_eq!(
            graph.issues()[0].to_string(),
            "task \"b\" declares missing prerequisite \"missing\""
        );
    }

    // -- ready_set predicates -------------------------------------------------

    #[test]
    fn store_done_prereq_makes_unlisted_dependent_runnable() {
        let graph = WorkGraph::build([item("B", &["A"])]).graph;
        let states = state_map(&[("B", "fresh")]);

        assert!(ready_ids(&graph, &states, &ReadySetQuery::default()).is_empty());
        let satisfied = ReadySetQuery::new().with_satisfied_prerequisites(["A"]);
        assert_eq!(ready_ids(&graph, &states, &satisfied), ["B"]);
        let other = ReadySetQuery::new().with_satisfied_prerequisites(["other"]);
        assert!(ready_ids(&graph, &states, &other).is_empty());
    }

    #[test]
    fn fully_listed_graph_is_byte_identical_with_satisfied_set() {
        let graph = WorkGraph::build([item("A", &[]), item("B", &["A"])]).graph;
        let states = state_map(&[("A", "done"), ("B", "fresh")]);
        let baseline = ready_ids(&graph, &states, &ReadySetQuery::default());
        let with_set = ready_ids(
            &graph,
            &states,
            &ReadySetQuery::new().with_satisfied_prerequisites(["A"]),
        );
        assert_eq!(baseline, ["B"]);
        assert_eq!(with_set, baseline);
    }

    #[test]
    fn own_state_gates_eligibility() {
        let graph = WorkGraph::build([
            item("fresh", &[]),
            item("retryable", &[]),
            item("interrupted", &[]),
            item("running", &[]),
            item("done", &[]),
            item("failed", &[]),
            item("unknown", &[]),
        ])
        .graph;
        let states = state_map(&[
            ("fresh", "fresh"),
            ("retryable", "retryable"),
            ("interrupted", "interrupted"),
            ("running", "running"),
            ("done", "done"),
            ("failed", "failed"),
        ]);
        assert_eq!(
            ready_ids(&graph, &states, &ReadySetQuery::default()),
            ["fresh", "retryable", "interrupted"]
        );
    }

    #[test]
    fn listed_prereq_must_be_done_not_merely_present() {
        let graph = WorkGraph::build([item("A", &[]), item("B", &["A"])]).graph;
        for state in ["fresh", "running", "failed", "interrupted"] {
            let states = state_map(&[("A", state), ("B", "fresh")]);
            assert!(
                !ready_ids(&graph, &states, &ReadySetQuery::default()).contains(&"B".to_string())
            );
        }
        // A listed-but-undone prerequisite is not rescued by the satisfied
        // set naming it: the node is present and authoritative.
        let states = state_map(&[("A", "failed"), ("B", "fresh")]);
        let query = ReadySetQuery::new().with_satisfied_prerequisites(["A"]);
        assert_eq!(ready_ids(&graph, &states, &query), ["B"]);
    }

    #[test]
    fn excluded_id_is_skipped_but_still_satisfies_dependents() {
        let graph = WorkGraph::build([item("A", &[]), item("B", &["A"]), item("C", &[])]).graph;
        let states = state_map(&[("A", "done"), ("B", "fresh"), ("C", "fresh")]);
        let query = ReadySetQuery::new().with_excluded(["C"]);
        assert_eq!(ready_ids(&graph, &states, &query), ["B"]);

        // Excluding a still-fresh A hides A but does not mark it done for B.
        let states = state_map(&[("A", "fresh"), ("B", "fresh"), ("C", "fresh")]);
        let query = ReadySetQuery::new().with_excluded(["A"]);
        assert_eq!(ready_ids(&graph, &states, &query), ["C"]);
    }

    #[test]
    fn capabilities_must_be_a_subset_of_the_workers() {
        let gpu = item("gpu", &[]).with_required_capabilities(["gpu"]);
        let both = item("both", &[]).with_required_capabilities(["gpu", "docker"]);
        let plain = item("plain", &[]);
        let graph = WorkGraph::build([gpu, both, plain]).graph;
        let states = fresh_states(&graph);

        assert_eq!(
            ready_ids(&graph, &states, &ReadySetQuery::default()),
            ["plain"]
        );
        let gpu_worker = ReadySetQuery::new().with_worker_capabilities(["gpu"]);
        assert_eq!(ready_ids(&graph, &states, &gpu_worker), ["gpu", "plain"]);
        let full = ReadySetQuery::new().with_worker_capabilities(["gpu", "docker", "extra"]);
        assert_eq!(ready_ids(&graph, &states, &full), ["gpu", "both", "plain"]);
    }

    #[test]
    fn ready_set_orders_by_descending_priority_with_stable_walk_ties() {
        let graph = WorkGraph::build([
            item("w0", &[]).with_priority(0),
            item("h1", &[]).with_priority(5),
            item("w1", &[]).with_priority(0),
            item("h2", &[]).with_priority(5),
            item("low", &[]).with_priority(-3),
            item("top", &[]).with_priority(9),
        ])
        .graph;
        let states = fresh_states(&graph);
        assert_eq!(
            ready_ids(&graph, &states, &ReadySetQuery::default()),
            ["top", "h1", "h2", "w0", "w1", "low"]
        );
    }

    #[test]
    fn ready_set_accepts_any_as_ref_str_state_type() {
        #[derive(Clone, Copy)]
        enum TaskState {
            Fresh,
            Done,
        }
        impl AsRef<str> for TaskState {
            fn as_ref(&self) -> &str {
                match self {
                    TaskState::Fresh => "fresh",
                    TaskState::Done => "done",
                }
            }
        }
        let graph = WorkGraph::build([item("A", &[]), item("B", &["A"])]).graph;
        let states: HashMap<String, TaskState> = HashMap::from([
            ("A".to_string(), TaskState::Done),
            ("B".to_string(), TaskState::Fresh),
        ]);
        assert_eq!(ready_ids(&graph, &states, &ReadySetQuery::default()), ["B"]);
    }

    // -- structure queries ----------------------------------------------------

    #[test]
    fn edges_and_topological_order_follow_resolved_prerequisites() {
        let graph = WorkGraph::build([
            item("leaf", &["mid", "ghost"]),
            item("mid", &["root"]),
            item("root", &[]),
            item("solo", &[]),
        ])
        .graph;
        assert_eq!(graph.topological_order(), ["root", "solo", "mid", "leaf"]);
        let edges = graph.edges();
        assert_eq!(edges["leaf"], set(&["mid"]));
        assert_eq!(edges["mid"], set(&["root"]));
        assert!(edges["root"].is_empty());
        assert_eq!(graph.prerequisites_of("leaf"), set(&["mid"]));
        assert!(graph.resolved_prerequisites("nope").is_empty());
        assert!(graph.declared_prerequisites("nope").is_empty());
        assert!(graph.contains("mid") && !graph.contains("ghost"));
        assert_eq!(graph.get("mid").unwrap().id(), "mid");
        assert_eq!(graph.len(), 4);
        assert!(WorkGraph::default().is_empty());
        assert_eq!(graph.issues().len(), 1);
    }

    // -- multi-source aggregation ---------------------------------------------

    struct MemorySource {
        items: Vec<WorkItem>,
        fail: bool,
    }

    impl WorkSource for MemorySource {
        fn source_kind(&self) -> &str {
            "memory"
        }
        fn source_name(&self) -> String {
            "mem".into()
        }
        fn list_work(&self) -> Result<Vec<WorkItem>, WorkSourceError> {
            if self.fail {
                return Err(WorkSourceError::new("mem: tracker unreachable"));
            }
            Ok(self.items.clone())
        }
        fn report(&self, _report: &WorkReport) -> Result<(), WorkSourceError> {
            Ok(())
        }
    }

    #[test]
    fn builder_validates_over_the_aggregate_in_argument_order() {
        let a = MemorySource {
            items: vec![item("dependent", &["base"]), item("orphan", &["nowhere"])],
            fail: false,
        };
        let b = MemorySource {
            items: vec![item("base", &[])],
            fail: false,
        };
        let sources: Vec<&dyn WorkSource> = vec![&a, &b];
        let result = WorkGraphBuilder::build(sources).unwrap();

        assert_eq!(
            result
                .graph
                .items()
                .iter()
                .map(WorkItem::id)
                .collect::<Vec<_>>(),
            ["dependent", "orphan", "base"]
        );
        // The cross-source edge resolved; only the truly unresolved one is
        // an issue.
        assert_eq!(result.graph.prerequisites_of("dependent"), set(&["base"]));
        assert_eq!(
            result.issues,
            vec![GraphValidationIssue {
                referencing_id: "orphan".into(),
                missing_id: "nowhere".into()
            }]
        );
        assert!(result.excluded.is_empty());

        // A single source is the degenerate aggregation.
        let boxed: Vec<Box<dyn WorkSource>> = vec![Box::new(MemorySource {
            items: vec![item("base", &[])],
            fail: false,
        })];
        let single = WorkGraphBuilder::build_boxed(&boxed).unwrap();
        assert_eq!(single.graph.len(), 1);
    }

    #[test]
    fn builder_isolates_cross_source_duplicate_and_propagates_listing_errors() {
        let a = MemorySource {
            items: vec![item("shared", &[]), item("only-a", &[])],
            fail: false,
        };
        let b = MemorySource {
            items: vec![item("shared", &[])],
            fail: false,
        };
        let result = WorkGraphBuilder::build([&a as &dyn WorkSource, &b]).unwrap();
        assert_eq!(item_ids(&result.graph), set(&["only-a"]));
        assert_eq!(result.excluded[0].task_id, "shared");

        let failing = MemorySource {
            items: vec![],
            fail: true,
        };
        let err = WorkGraphBuilder::build([&a as &dyn WorkSource, &failing]).unwrap_err();
        assert_eq!(err.to_string(), "mem: tracker unreachable");
    }
}
