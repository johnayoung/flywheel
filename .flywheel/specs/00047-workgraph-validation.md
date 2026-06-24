# Feature: WorkGraph (explicit, validated task DAG)

## Outcome
The orchestrator builds an explicit, validated `WorkGraph` from the existing
`WorkItem` objects a `WorkSource` (or several) yields. Structural corruption —
duplicate task ids, a self-dependency, a cycle — fails loudly with a
`WorkGraphValidationError` whose message names the offending ids *before* any
scheduling pass runs, instead of producing a graph that schedules wrong or
not at all. A dangling/cross-source-missing prerequisite is surfaced as a
named `GraphValidationIssue` while the referencing task stays ineligible
(today's behavior). Scheduling reads runnable items from the validated graph
via a `ready_set(...)`, and `select_next_task`'s deterministic
first-eligible ordering is unchanged.

## Background
Today the task DAG is implicit: prerequisite edges ride along on each
`WorkItem`, and `select_next_task` resolves them ad hoc — a missing
prerequisite silently makes a task ineligible, and a duplicate id, a
self-loop, or a cycle is never detected at all (it just yields a graph that
deadlocks or schedules an item that can never satisfy its own precondition).
The roadmap wants the DAG to be a first-class object that is validated once,
loudly, before scheduling. The tacit requirement the wording hides: the four
defect classes are NOT one bucket. Duplicate-id / self-dependency / cycle are
*structural corruption* — no safe schedule exists, so building a graph at all
is a lie; these must hard-fail. A *missing* prerequisite is a different class:
under multi-source aggregation the referenced work may simply not be loaded
yet by a sibling source on this pass, so aborting the whole build would both
regress the two committed tests that pin "dangling prereq never runs" and
break the legitimate multi-source case. That split is the load-bearing design
decision (D-1) and every criterion below inherits it.

## Scope
### In scope
- A `WorkGraph` value built from a sequence of existing `WorkItem` objects.
- A `WorkGraphBuilder.build()` that aggregates `list_work()` across one or
  more `WorkSource`s, constructs the graph, validates it, and raises
  `WorkGraphValidationError` on structural defects (duplicate id, self-dep,
  cycle).
- A `GraphValidationResult` carrying zero or more `GraphValidationIssue`
  records (the non-fatal class: missing/dangling prerequisites, including
  cross-source-missing references).
- `WorkGraph.ready_set(states, excluded)` returning every currently-runnable
  item (prerequisites all DONE, own state eligible, id not excluded).
- The scheduler selecting from the validated graph while preserving
  `select_next_task`'s deterministic first-eligible-in-walk-order semantics.

### Out of scope
- Any change to core `flywheel_core.task.Task` (it gains no prerequisites
  field; it stays input-source agnostic).
- Any new field on `WorkItem` (provenance, priority, weight — those belong to
  later specs 00048/00049).
- Priority ordering, weighting, or a `Scheduler.choose` abstraction (00049):
  selection stays deterministic first-eligible.
- The distributed claim-fallback ("claim fails -> try next eligible task")
  already shipped at `_orchestrate.py:839-933` — not re-specced here.

### Must not regress
- `test_select_next_picks_first_fresh_in_walk_order`,
  `test_select_next_respects_prerequisites`,
  `test_select_next_returns_none_when_prereq_missing`,
  `test_select_next_returns_none_when_all_done`,
  `test_select_next_spans_phases` (and the other `select_next_*` tests) in
  `packages/flywheel-orchestrator/tests/test_workflow.py` — all stay green
  unchanged, including the silently-ineligible-on-missing-prereq behavior.
- `test_task_with_dangling_prerequisite_never_runs` in
  `packages/flywheel-orchestrator/tests/test_orchestrator.py` — a dangling
  prerequisite still yields `report.runs == ()` (the task never dispatches);
  the graph surfaces the dangling edge as an issue but does NOT abort the
  pass.
- The single-`DirectoryWorkSource` common case behaves exactly as today
  (same selection order, same eligibility, same orchestrate outcomes).

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its
grader type, visibility, the exact check, and the gaming move it forecloses.

1. When a graph is built from a simple chain (a -> b -> c, each prereq the
   prior), the build succeeds and the graph reports the chain's edges and a
   topological order consistent with them. [command | held-out]
   verify: a held-out test in `test_work_graph.py` builds the chain from
   `WorkItem`s and asserts `build()` returns without raising and the graph
   exposes b's prerequisite as {a}, c's as {b}; `uv run pytest
   packages/flywheel-orchestrator/tests/test_work_graph.py` exits 0.
   defends against: a `build()` that accepts everything by never actually
   recording edges — the edge-set assertion fails a no-op builder.

2. When a graph is built from a fan-out (root with two independent children
   depending only on root), the build succeeds and both children resolve
   root as their sole prerequisite. [command | held-out]
   verify: held-out test asserts `build()` does not raise and each child's
   resolved prerequisites == {root}; pytest exits 0.
   defends against: a builder that only handles linear chains and drops the
   second edge of a fan-out.

3. When a graph is built from a fan-in (two independent roots, one child
   depending on both), the build succeeds and the child resolves both roots
   as prerequisites. [command | held-out]
   verify: held-out test asserts `build()` does not raise and the child's
   resolved prerequisites == {root_a, root_b}; pytest exits 0.
   defends against: a builder that keeps only the first prerequisite per node
   (a single-parent assumption) and silently loses the second edge.

4. If two distinct `WorkItem`s carry the same task id, then `build()` raises
   `WorkGraphValidationError` and the message names the duplicated id.
   [command | held-out]
   verify: held-out test uses `pytest.raises(WorkGraphValidationError,
   match="<dup-id>")`; pytest exits 0.
   defends against: deduping silently (last-wins / first-wins) so a real
   collision is swallowed; and raising a generic error with no id, so the
   operator cannot find the offender. The `match=` on the id forecloses both.

5. If a `WorkItem` lists its own task id among its prerequisites, then
   `build()` raises `WorkGraphValidationError` and the message names that id.
   [command | held-out]
   verify: held-out test asserts `pytest.raises(WorkGraphValidationError,
   match="<self-id>")`; pytest exits 0.
   defends against: treating a self-edge as a 1-cycle that's "harmless" and
   letting the task sit permanently ineligible (a silent deadlock) instead of
   rejecting the corrupt definition.

6. If the prerequisite edges contain a cycle, then `build()` raises
   `WorkGraphValidationError` and the message names every id participating in
   the cycle. [command | held-out]
   verify: held-out test builds a -> b -> a (and a 3-node a -> b -> c -> a),
   asserts `WorkGraphValidationError` is raised and that each cycle member id
   appears in the message; pytest exits 0.
   defends against: detecting "a cycle exists" but reporting no members
   (operator cannot act); and detecting only 2-cycles while a 3-cycle slips
   through. Asserting every member is named on a 3-node cycle forecloses both.

7. If a `WorkItem`'s prerequisite id resolves to no item in the aggregated
   graph, then `build()` does NOT raise, the returned
   `GraphValidationResult` contains a `GraphValidationIssue` naming the
   referencing task id and the missing prerequisite id, and the referencing
   task is absent from `ready_set(...)`. [command | held-out]
   verify: held-out test asserts `build()` returns normally, the result's
   issues include one whose fields name (referencing_id, missing_id), and
   `ready_set` over that graph excludes the referencing task; pytest exits 0.
   defends against: the two opposite cheats — (a) hard-failing the whole
   build on a missing prereq (which would regress the must-not-regress
   tests), and (b) staying silent with no issue at all (the operator never
   learns the edge is dangling). The criterion pins BOTH a non-raise AND a
   recorded issue, so neither cheat passes.

8. While no task is DONE, `ready_set(...)` over a chain/fan-out graph returns
   exactly the root(s) — every node with no prerequisites and an eligible
   state — and no child. [command | held-out]
   verify: held-out test seeds all states eligible (none DONE), asserts
   `ready_set` returns precisely the prerequisite-free roots; pytest exits 0.
   defends against: a `ready_set` that returns every eligible node regardless
   of prerequisites (ignores edges) — it would wrongly include children.

9. When a parent reaches DONE, `ready_set(...)` returns that parent's child
   (now its only prerequisite is satisfied) and no longer returns the parent.
   [command | held-out]
   verify: held-out test marks the parent DONE, asserts the child is now in
   `ready_set` and the DONE parent is not; pytest exits 0.
   defends against: a `ready_set` that ignores the DONE transition and keeps
   returning the parent, or never promotes the child (treats DONE as still
   blocking).

10. When the shared parent of two children reaches DONE, `ready_set(...)`
    returns both parallel children. [command | held-out]
    verify: held-out test (fan-out, parent DONE) asserts both children are in
    `ready_set`; pytest exits 0.
    defends against: a `ready_set` that returns only the first satisfied child
    and stops (an early-return bug that would serialize parallel work).

11. When a task is excluded (in the `excluded` set) or its state is
    claimed/running/not-eligible, `ready_set(states, excluded)` omits it even
    if its prerequisites are all DONE. [command | held-out]
    verify: held-out test marks a root's prerequisites satisfied but passes it
    in `excluded` (and separately gives it a non-eligible state), asserts it
    is absent from `ready_set` in both cases; pytest exits 0.
    defends against: a `ready_set` that grades only on prerequisites and
    ignores the `excluded` set / own state — it would re-offer a task a peer
    worker already holds, double-dispatching it.

12. When `build()` aggregates two `WorkSource`s, the items from both combine
    into one graph and a prerequisite satisfied by an item from the *other*
    source resolves as a real edge (no issue raised for it). [command |
    held-out]
    verify: held-out test builds from two `DirectoryWorkSource`-shaped
    sources where source A's task depends on source B's task; asserts
    `build()` does not raise, the cross-source edge resolves, and the
    result's issues do not name that pair; pytest exits 0.
    defends against: a builder that validates each source in isolation and
    reports a cross-source edge as "missing" — the per-source-validation cheat
    that would make legitimate multi-source DAGs un-runnable.

13. When `build()` aggregates two `WorkSource`s and a prerequisite resolves to
    no item in EITHER source, the missing reference is surfaced as a
    `GraphValidationIssue` (per D-1) and `build()` does not raise.
    [command | held-out]
    verify: held-out test asserts a truly-unresolved cross-source prerequisite
    produces an issue naming the (referencing_id, missing_id) and `build()`
    returns normally; pytest exits 0.
    defends against: promoting "missing after aggregation" to a hard-fail
    (regressing the dangling-prereq contract) OR dropping it silently (the
    operator never sees the unresolved edge). Pins non-raise + recorded issue.

14. When the orchestrator schedules over the validated graph for a single
    `DirectoryWorkSource`, `select_next_task`'s deterministic
    first-eligible-in-walk-order selection is byte-for-byte unchanged.
    [command | held-out] (verification-surface)
    verify: the full existing selection + orchestrator suites pass unchanged
    as one command: `uv run pytest
    packages/flywheel-orchestrator/tests/test_workflow.py
    packages/flywheel-orchestrator/tests/test_orchestrator.py`; exit 0 with
    no assertion edited or deleted.
    defends against: rewriting selection to "any ready item" (losing walk
    order) or quietly relaxing the missing-prereq tests to make the new graph
    fit — the existing committed assertions are the holdout.

15. When the orchestrator's scheduling pass and reconcile pass consume the
    validated graph, the full orchestrator test surface still passes as one
    composed run. [command | held-out] (verification-surface)
    verify: `uv run pytest packages/flywheel-orchestrator/tests/` exits 0,
    with no test file under `tests/` modified and no assertion weakened.
    defends against: a per-task "make my new graph test pass" handler that
    never integrates into the real `_orchestrate.py` scheduling/reconcile
    loops — the composed orchestrator suite is the seam holdout that the
    integration actually wired in.

Verification surface: this feature changes the scheduling/selection path —
the machinery `select_next_task` and the orchestrate loop use to decide what
runs. The existing selection + orchestrator suites still run and still pass
after the change; no assertion in `test_workflow.py` or `test_orchestrator.py`
is relaxed, removed, or skipped. New behavior (graph construction +
validation + `ready_set`) is proven by held-out tests in a new
`test_work_graph.py` the implementing agent does not author against its own
known answers; the authoritative grade for the must-not-regress contract is
the unchanged committed suites (criteria #14, #15). No check is removed, so no
replacement is owed.

## Decomposition Hint (for /fw-plan)
Splits along four architectural layers; chain them with prerequisites.
- Layer graph-model + validation (the new module: `WorkGraph`,
  `GraphValidationResult`, `GraphValidationIssue`, `WorkGraphValidationError`,
  and the validate-on-construct logic): satisfies #1, #2, #3, #4, #5, #6, #7.
- Layer ready_set (the runnable-item query over a built graph): satisfies #8,
  #9, #10, #11; depends on graph-model.
- Layer builder/aggregation (`WorkGraphBuilder.build()` over one-or-more
  `WorkSource`s): satisfies #12, #13; depends on graph-model + ready_set.
- Layer scheduler-integration (the orchestrate scheduling + reconcile path
  consumes the validated graph while preserving first-eligible selection):
  satisfies #14, #15; depends on builder/aggregation. This is the seam — its
  authoritative grader runs the existing selection + orchestrator suites
  together.

Shared invariants the layers assert against, name them so dependent tasks move
together: the exact symbol names `WorkGraph`, `WorkGraphBuilder`,
`GraphValidationResult`, `GraphValidationIssue`, `WorkGraphValidationError`;
the `ready_set(states, excluded)` signature (a states mapping/lookup of
task-id -> eligibility and an `excluded` id set); and the D-1 split
(structural defect -> raise; missing prerequisite -> issue + ineligible).

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-1: Missing prerequisite is a non-fatal issue; structural corruption hard-fails  (Status: Accepted)
- Context: the roadmap asks that "missing prerequisites fail clearly," but two
  committed tests pin that a dangling prerequisite simply makes a task
  ineligible and it never runs
  (`test_select_next_returns_none_when_prereq_missing`,
  `test_task_with_dangling_prerequisite_never_runs`). Multi-source
  aggregation is in scope, so a prerequisite unresolved within one source's
  `list_work()` may be satisfied by a sibling source on the same pass — it is
  not necessarily corrupt.
- Decision: `WorkGraphBuilder.build()` HARD-FAILS (raises
  `WorkGraphValidationError`, message naming the offending ids) on the
  structural-corruption class — duplicate task id, self-dependency, cycle —
  because no safe schedule exists for those. A MISSING prerequisite (after
  full aggregation across all sources) is a DIFFERENT class: `build()` does
  NOT raise; it records a `GraphValidationIssue` naming the referencing and
  missing ids on the returned `GraphValidationResult`, and the referencing
  task stays ineligible / out of `ready_set` exactly as today. "Fail clearly"
  is satisfied by the surfaced, named issue — not by aborting the pass.
- Rejected: (a) Hard-fail on missing prereq too. Lost because it would regress
  both committed tests and abort a legitimate multi-source DAG whenever any
  source is temporarily empty or lagging — a single missing edge would
  freeze the entire batch. (b) Treat ALL four defect classes as non-fatal
  issues. Lost because a duplicate id, self-loop, or cycle has no correct
  schedule; continuing past it produces a graph that deadlocks or
  double-defines a task — silent corruption is exactly what this spec exists
  to end.
- Consequences: the two must-not-regress tests stay green unchanged (no
  supersession needed). The negative: a genuinely-orphaned task (its
  prerequisite will never be produced by any source) is reported as an issue
  but is not itself an error that stops the batch — an operator must read the
  issue list to notice it. That is the accepted cost of not regressing the
  silently-ineligible contract; the named issue is what makes the orphan
  discoverable rather than invisible.

### D-2: Core Task and WorkItem field set are unchanged  (Status: Accepted)
- Context: prerequisites are an orchestration concept already carried on
  `WorkItem.prerequisites`; core `Task` is deliberately input-source agnostic
  and has no prerequisites field.
- Decision: the WorkGraph is built FROM the existing `WorkItem` objects. No
  field is added to `WorkItem` and nothing is added to core `Task` by this
  spec. Provenance/priority/weight belong to later specs (00048/00049).
- Rejected: adding a `graph`/`depends_on` field to `Task` (breaks the purity
  + input-agnostic invariant) or to `WorkItem` (premature; out of scope here).
- Consequences: the graph is a pure derivation of what sources already yield;
  it can be introduced with zero schema migration and zero change to loaders.

## Open Questions (accepted gaps)
None. Every criterion lowers to a `command` grader.

## Next Steps
Run `/fw-plan 00047-workgraph-validation` to compile these criteria into
flywheel tasks and graders.
