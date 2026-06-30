"""WorkGraph — an explicit, validated task DAG over ``WorkItem`` objects.

The orchestrator's prerequisite edges ride along on each
:class:`~flywheel_orchestrator._sources.WorkItem`
(``WorkItem.prerequisites``). This module lifts those implicit edges into a
first-class, validated object so structural corruption is caught once,
loudly, *before* any scheduling pass — instead of silently producing a graph
that deadlocks or schedules a task that can never satisfy its own
precondition.

Every defect is *contained, never fatal* — the containment floor (spec
00065) extends the missing-prerequisite posture (spec 00047, decision D-1)
to structural corruption so one corrupt definition never poisons the
unrelated tasks sharing its scheduling pass:

* **Structural corruption** — a duplicate task id, a self-dependency, or a
  cycle — isolates the offending task(s). Construction does NOT raise; it
  records an :class:`ExcludedTask` whose ``reason`` names the offending
  id(s) — for a cycle, every participating member — and builds the graph
  from the survivors. The isolated ids never reach :attr:`WorkGraph.items`,
  so they stay out of :meth:`WorkGraph.ready_set` while every valid,
  independent task in the same pass remains schedulable. The reason is
  recoverable on :attr:`WorkGraph.excluded`, never silently dropped.
* **A missing prerequisite** — an edge that resolves to no node in the
  graph. Under multi-source aggregation the referenced work may simply not
  be loaded by a sibling source on this pass, so aborting the build would
  regress the "dangling prerequisite never runs" contract. Construction
  records a :class:`GraphValidationIssue` naming the referencing and missing
  ids, and the referencing task stays out of :meth:`WorkGraph.ready_set`. A
  survivor whose prerequisite was *isolated* above keeps a dangling edge of
  exactly this class.

``ready_set(states, excluded)`` answers the same eligibility question
``flywheel_orchestrator._workflow.select_next_task`` does — a task is runnable
when every prerequisite is DONE, its own state is eligible
(fresh/retryable/interrupted), and its id is not excluded — but returns
*every* such item rather than the first, so parallel children all surface.
"""

from __future__ import annotations

from collections.abc import Iterable, Mapping
from dataclasses import dataclass
from typing import Any

from flywheel_orchestrator._sources import WorkItem, WorkSource

# Eligibility mirrors ``TaskState`` (see ``_workflow.TaskState`` ~110-118)
# without importing it, keeping this graph-model module decoupled from the
# scheduler's heavyweight import graph (and free of the import cycle the
# scheduler-integration layer would otherwise create when it consumes the
# graph). ``TaskState`` is a ``(str, Enum)``, so its members compare equal to
# these values; callers may equally pass the raw string values.
_ELIGIBLE_STATE_VALUES: frozenset[str] = frozenset(
    {"fresh", "retryable", "interrupted"}
)
_DONE_STATE_VALUE = "done"


def _state_value(state: Any) -> Any:
    """Normalize a state lookup to its comparable value.

    Accepts a ``TaskState`` member (``str`` enum — returns its ``.value``),
    a bare string, or ``None`` (unknown state). Returning ``.value`` makes
    eligibility checks identity-independent of which ``TaskState`` class
    object the caller imported.
    """
    return getattr(state, "value", state)


@dataclass(frozen=True, kw_only=True)
class ExcludedTask:
    """A task isolated from a :class:`WorkGraph` for structural corruption.

    Recorded (never raised) when construction encounters a duplicate task id,
    a self-dependency, or a cycle: ``task_id`` is the isolated task and
    ``reason`` is a human-readable explanation that always names the offending
    id(s) — for a cycle, every participating member. The isolated task is
    absent from :attr:`WorkGraph.items` and the ready set, while every valid,
    independent task in the same pass stays schedulable. The reason keeps the
    exclusion recoverable rather than a silent drop (spec 00065).
    """

    task_id: str
    reason: str


@dataclass(frozen=True, kw_only=True)
class GraphValidationIssue:
    """A non-fatal defect surfaced while building a :class:`WorkGraph`.

    The only such class today is a dangling prerequisite: ``referencing_id``
    is the task that declared the edge, ``missing_id`` the prerequisite id
    that resolves to no item in the graph. The issue is recorded (never
    raised) and the referencing task stays out of :meth:`WorkGraph.ready_set`.
    """

    referencing_id: str
    missing_id: str

    @property
    def task_id(self) -> str:
        """Alias for :attr:`referencing_id` (the edge's owner)."""
        return self.referencing_id

    @property
    def missing_prerequisite(self) -> str:
        """Alias for :attr:`missing_id` (the unresolved prerequisite)."""
        return self.missing_id


@dataclass(frozen=True, kw_only=True)
class GraphValidationResult:
    """A built :class:`WorkGraph` together with its recorded defects.

    Returned by :meth:`WorkGraph.build`. ``graph`` is the validated graph
    built from the survivors; ``issues`` holds the recorded
    missing-prerequisite findings (empty when every edge resolved);
    ``excluded`` holds the tasks isolated for structural corruption (a
    duplicate id, a self-dependency, or a cycle), each with a reason naming
    the offender (empty when the set was structurally sound).
    """

    graph: "WorkGraph"
    issues: tuple[GraphValidationIssue, ...] = ()
    excluded: tuple[ExcludedTask, ...] = ()


class WorkGraph:
    """A validated prerequisite DAG built from a sequence of ``WorkItem``.

    Construction validates structure eagerly but *contains* every defect
    rather than raising (spec 00065). A duplicate task id, a self-dependency,
    or a cycle isolates the offending task(s): the graph is built from the
    survivors and each exclusion is recorded as an :class:`ExcludedTask` on
    :attr:`excluded` with a reason naming the offender(s). Missing
    prerequisites are recorded as :class:`GraphValidationIssue`\\ s on
    :attr:`issues`. Either way, every valid, independent task in the same
    pass stays schedulable.
    """

    def __init__(self, items: Iterable[WorkItem] = ()) -> None:
        materialized = tuple(items)

        # Structural corruption is contained, not fatal: an offending task is
        # isolated with a recorded reason and the graph is built from the
        # survivors, so one corrupt definition never poisons the unrelated
        # tasks sharing its scheduling pass. ``excluded`` keeps the reasons
        # (recoverable, never silently dropped); an isolated id never reaches
        # ``_by_id`` and so stays out of every structure query and the ready
        # set.
        excluded: list[ExcludedTask] = []
        excluded_ids: set[str] = set()

        # -- duplicate task id (structural corruption) ---------------------
        # An id defined more than once is ambiguous -- nothing can say which
        # definition is authoritative -- so every copy is isolated and the id
        # is named once. First-seen order keeps the record deterministic.
        counts: dict[str, int] = {}
        first_seen: list[str] = []
        for item in materialized:
            task_id = item.task.id
            if task_id not in counts:
                first_seen.append(task_id)
            counts[task_id] = counts.get(task_id, 0) + 1
        for task_id in first_seen:
            if counts[task_id] > 1:
                excluded_ids.add(task_id)
                excluded.append(
                    ExcludedTask(
                        task_id=task_id,
                        reason=(
                            f"duplicate task id: {task_id!r} is defined "
                            f"{counts[task_id]} times in the same scheduling "
                            f"pass"
                        ),
                    )
                )

        # -- self-dependency (structural corruption) -----------------------
        # A task that lists itself as a prerequisite can never satisfy its own
        # precondition; isolate it. A copy already isolated as a duplicate is
        # not recorded twice.
        for item in materialized:
            task_id = item.task.id
            if task_id in excluded_ids:
                continue
            if task_id in tuple(item.prerequisites):
                excluded_ids.add(task_id)
                excluded.append(
                    ExcludedTask(
                        task_id=task_id,
                        reason=(
                            f"self-dependency: task {task_id!r} lists itself "
                            f"as a prerequisite"
                        ),
                    )
                )

        # -- cycle detection (structural corruption) -----------------------
        # Detect cycles over the survivors' resolved edges, then isolate every
        # participating member -- naming the whole cycle in each member's
        # reason so the corrupt loop is recoverable from any of its ids.
        survivor_ids = {
            item.task.id
            for item in materialized
            if item.task.id not in excluded_ids
        }
        resolved_for_cycle = {
            item.task.id: tuple(
                p for p in item.prerequisites if p in survivor_ids
            )
            for item in materialized
            if item.task.id not in excluded_ids
        }
        cycle_members = _cycle_members(resolved_for_cycle)
        if cycle_members:
            named = ", ".join(sorted(cycle_members))
            for member in sorted(cycle_members):
                excluded_ids.add(member)
                excluded.append(
                    ExcludedTask(
                        task_id=member,
                        reason=f"dependency cycle among task id(s): {named}",
                    )
                )

        # -- build the graph from the survivors ----------------------------
        # ``_declared`` keeps every prerequisite as authored (used to keep a
        # task with a dangling edge ineligible); ``_resolved`` keeps only
        # edges that point at a surviving node (used for cycle detection,
        # topological order, and the resolved-prerequisite query). A survivor
        # whose prerequisite was isolated above keeps a dangling edge --
        # recorded as a GraphValidationIssue exactly like an unresolved
        # cross-source reference -- so it stays out of the ready set.
        survivors = tuple(
            item for item in materialized if item.task.id not in excluded_ids
        )
        by_id: dict[str, WorkItem] = {
            item.task.id: item for item in survivors
        }
        declared: dict[str, tuple[str, ...]] = {}
        resolved: dict[str, tuple[str, ...]] = {}
        issues: list[GraphValidationIssue] = []
        for item in survivors:
            task_id = item.task.id
            prereqs = tuple(item.prerequisites)
            declared[task_id] = prereqs
            resolved[task_id] = tuple(p for p in prereqs if p in by_id)
            for prereq_id in prereqs:
                if prereq_id not in by_id:
                    issues.append(
                        GraphValidationIssue(
                            referencing_id=task_id, missing_id=prereq_id
                        )
                    )

        self._items: tuple[WorkItem, ...] = survivors
        self._by_id = by_id
        self._declared = declared
        self._resolved = resolved
        self._issues: tuple[GraphValidationIssue, ...] = tuple(issues)
        self._excluded: tuple[ExcludedTask, ...] = tuple(excluded)

    # -- builder --------------------------------------------------------------

    @classmethod
    def build(cls, items: Iterable[WorkItem] = ()) -> GraphValidationResult:
        """Construct the graph and return it with its recorded defects.

        Never raises: structural corruption (a duplicate id, a
        self-dependency, a cycle) isolates the offending task(s) and missing
        prerequisites are recorded. Returns a :class:`GraphValidationResult`
        carrying the survivor graph, any missing-prerequisite ``issues``, and
        any ``excluded`` tasks (each with a reason naming the offender).
        """
        graph = cls(items)
        return graph.validation

    # -- structure queries ----------------------------------------------------

    @property
    def items(self) -> tuple[WorkItem, ...]:
        """The graph's items, in construction (walk) order."""
        return self._items

    @property
    def issues(self) -> tuple[GraphValidationIssue, ...]:
        """Recorded non-fatal issues (missing prerequisites)."""
        return self._issues

    @property
    def excluded(self) -> tuple[ExcludedTask, ...]:
        """Tasks isolated for structural corruption, each with a reason.

        One :class:`ExcludedTask` per task isolated for a duplicate id, a
        self-dependency, or a cycle; its ``reason`` names the offender(s).
        Empty when the input set was structurally sound. The excluded ids are
        absent from :attr:`items` and the ready set.
        """
        return self._excluded

    @property
    def validation(self) -> GraphValidationResult:
        """This graph paired with its recorded issues and exclusions."""
        return GraphValidationResult(
            graph=self, issues=self._issues, excluded=self._excluded
        )

    def resolved_prerequisites(self, task_id: str) -> frozenset[str]:
        """Prerequisite ids of ``task_id`` that resolve to a real node.

        Dangling edges are excluded (they appear in :attr:`issues`).
        Returns an empty set for a prerequisite-free or unknown task.
        """
        return frozenset(self._resolved.get(task_id, ()))

    def prerequisites_of(self, task_id: str) -> frozenset[str]:
        """Alias for :meth:`resolved_prerequisites`."""
        return self.resolved_prerequisites(task_id)

    @property
    def edges(self) -> dict[str, frozenset[str]]:
        """Resolved prerequisite edges as ``{task_id: {prereq_id, ...}}``."""
        return {
            task_id: frozenset(prereqs)
            for task_id, prereqs in self._resolved.items()
        }

    @property
    def topological_order(self) -> tuple[str, ...]:
        """Task ids in a topological order (prerequisites before dependents).

        Defined over the resolved edges only; ties break by construction
        order. The graph is acyclic here (a cycle would have raised), so a
        total order always exists.
        """
        order = [item.task.id for item in self._items]
        indegree = {tid: len(self._resolved.get(tid, ())) for tid in order}
        dependents: dict[str, list[str]] = {tid: [] for tid in order}
        for tid in order:
            for prereq_id in self._resolved.get(tid, ()):
                dependents[prereq_id].append(tid)
        ready = [tid for tid in order if indegree[tid] == 0]
        out: list[str] = []
        while ready:
            tid = ready.pop(0)
            out.append(tid)
            for dependent in dependents[tid]:
                indegree[dependent] -= 1
                if indegree[dependent] == 0:
                    ready.append(dependent)
        return tuple(out)

    # -- scheduling query -----------------------------------------------------

    def ready_set(
        self,
        states: Mapping[str, Any],
        excluded: Iterable[str] = frozenset(),
        *,
        worker_capabilities: frozenset[str] = frozenset(),
    ) -> tuple[WorkItem, ...]:
        """Every currently-runnable item — and only those.

        An item is runnable when:

        * its id is not in ``excluded``, AND
        * its own state (``states[id]``) is eligible — fresh, retryable, or
          interrupted (mirrors ``select_next_task``), AND
        * every prerequisite resolves to a node whose state is DONE, AND
        * its ``required_capabilities`` is a subset of
          ``worker_capabilities``.

        ``states`` maps task id to its ``TaskState`` (or the equivalent
        string). A task with a dangling prerequisite, an unknown/ineligible
        own state, or an excluded id is omitted. Unlike ``select_next_task``,
        this returns *all* matches (no early return) so parallel children all
        surface.

        The returned items are ordered by descending ``priority``, ties
        broken by construction (walk) order via a *stable* sort (spec 00049,
        decision D-1) — so the caller's first element is the highest-priority
        runnable item, and an all-default (priority 0) set keeps pure walk
        order. ``worker_capabilities`` is the worker's advertised capability
        set (decision D-2): an item with empty ``required_capabilities`` is
        runnable by any worker, including one with an empty set; the default
        empty set preserves today's behavior for every existing zero-
        requirement item. Both behaviors apply in both execution modes.
        """
        excluded_ids = set(excluded)
        runnable: list[WorkItem] = []
        for item in self._items:
            task_id = item.task.id
            if task_id in excluded_ids:
                continue
            if _state_value(states.get(task_id)) not in _ELIGIBLE_STATE_VALUES:
                continue
            if not item.required_capabilities <= worker_capabilities:
                continue
            prereqs_satisfied = all(
                prereq_id in self._by_id
                and _state_value(states.get(prereq_id)) == _DONE_STATE_VALUE
                for prereq_id in self._declared.get(task_id, ())
            )
            if prereqs_satisfied:
                runnable.append(item)
        # Stable descending-priority sort: equal-priority items keep walk
        # order, so an all-default set is byte-identical to the prior
        # construction-order result.
        runnable.sort(key=lambda item: item.priority, reverse=True)
        return tuple(runnable)


class WorkGraphBuilder:
    """Aggregates ``list_work()`` across sources into one validated graph.

    The builder is the multi-source seam: it asks each :class:`WorkSource`
    for its current items, concatenates them into a single combined set, and
    only THEN hands that set to :meth:`WorkGraph.build`. Validation over the
    aggregate — never per source — is the load-bearing property (spec 00047,
    decision D-1): a prerequisite declared by an item from one source and
    satisfied by an item from another resolves as a real edge, while a
    reference unresolved in *every* source becomes a non-fatal
    :class:`GraphValidationIssue` rather than aborting the build. Structural
    corruption in the combined set (a duplicate id whose two members come from
    different sources, a self-dependency, a cycle) isolates the offending
    task(s) as :class:`ExcludedTask` records, exactly as constructing the
    model directly would -- the surviving items still build a graph.

    The builder depends only on the ``list_work()`` protocol, so it is
    source-kind agnostic: a :class:`DirectoryWorkSource`, a
    ``GithubWorkSource``, or any future adapter compose identically. A single
    source is just the degenerate aggregation — its graph is identical in
    edges and issues to building the model straight from that source's items.
    """

    @classmethod
    def build(
        cls,
        *sources: WorkSource | Iterable[WorkSource],
    ) -> GraphValidationResult:
        """Aggregate every source's items, then build and validate the graph.

        Accepts the sources either as positional arguments
        (``build(source_a, source_b)``) or as a single iterable
        (``build([source_a, source_b])``); both normalize to the same flat
        sequence. Each source's :meth:`WorkSource.list_work` is called once,
        in argument order, and the items are concatenated preserving that
        order so selection ties still break deterministically. The combined
        set is constructed through :meth:`WorkGraph.build`, which isolates
        structurally-corrupt tasks as :class:`ExcludedTask` records and
        records missing-prerequisite :class:`GraphValidationIssue`\\ s.
        """
        items: list[WorkItem] = []
        for source in _flatten_sources(sources):
            items.extend(source.list_work())
        return WorkGraph.build(items)


def _flatten_sources(
    sources: Iterable[WorkSource | Iterable[WorkSource]],
) -> list[WorkSource]:
    """Normalize varargs-or-single-iterable into a flat list of sources.

    A value carrying ``list_work`` is itself a source; any other iterable is
    a container of sources and is flattened one level. This lets callers pass
    ``build(a, b)`` or ``build([a, b])`` interchangeably without changing the
    aggregation semantics.
    """
    flat: list[WorkSource] = []
    for entry in sources:
        if hasattr(entry, "list_work"):
            flat.append(entry)  # type: ignore[arg-type]
        elif isinstance(entry, Iterable):
            flat.extend(entry)
        else:
            flat.append(entry)  # type: ignore[arg-type]
    return flat


def _cycle_members(resolved: Mapping[str, tuple[str, ...]]) -> frozenset[str]:
    """Return every node id that participates in a directed cycle.

    Edges point from a task to each of its prerequisites. Members are found
    as the union of all strongly-connected components of size > 1 (a 3-node
    cycle a->b->c->a yields ``{a, b, c}``). Self-loops are handled earlier as
    self-dependencies, so only multi-node cycles reach here. Tarjan's SCC
    algorithm, iterative to avoid recursion limits on long chains.
    """
    index_counter = 0
    index: dict[str, int] = {}
    lowlink: dict[str, int] = {}
    on_stack: dict[str, bool] = {}
    stack: list[str] = []
    members: set[str] = set()

    for root in resolved:
        if root in index:
            continue
        # Iterative DFS: each work frame is (node, iterator over its edges).
        work: list[tuple[str, Iterable[str]]] = [
            (root, iter(resolved.get(root, ())))
        ]
        index[root] = index_counter
        lowlink[root] = index_counter
        index_counter += 1
        stack.append(root)
        on_stack[root] = True
        while work:
            node, edges = work[-1]
            advanced = False
            for successor in edges:
                if successor not in index:
                    index[successor] = index_counter
                    lowlink[successor] = index_counter
                    index_counter += 1
                    stack.append(successor)
                    on_stack[successor] = True
                    work.append((successor, iter(resolved.get(successor, ()))))
                    advanced = True
                    break
                if on_stack.get(successor):
                    lowlink[node] = min(lowlink[node], index[successor])
            if advanced:
                continue
            # All successors explored: close this node's SCC if it is a root.
            if lowlink[node] == index[node]:
                component: list[str] = []
                while True:
                    member = stack.pop()
                    on_stack[member] = False
                    component.append(member)
                    if member == node:
                        break
                if len(component) > 1:
                    members.update(component)
            work.pop()
            if work:
                parent = work[-1][0]
                lowlink[parent] = min(lowlink[parent], lowlink[node])

    return frozenset(members)


__all__ = [
    "ExcludedTask",
    "GraphValidationIssue",
    "GraphValidationResult",
    "WorkGraph",
    "WorkGraphBuilder",
]
