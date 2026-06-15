---
name: fw-plan
description: Compile a spec or request into right-sized flywheel tasks, each spined on the strongest reward-hack-resistant grader the worker can run out-of-band
argument-hint: <NNNNN-FEATURE-name or free-text description>
---
<!-- managed-by: flywheel init -->

The grader is the spine of every task. A flywheel task states an outcome (`goal`) and how "done" is decided (`graders`); the agent plans its own approach, and the worker (`flywheel worker`) drives each task through the verification loop. Plan does two hard things, and they are the same decision made twice: **compile each spec criterion into the strongest grader the agent cannot reach or game, and right-size each task to a one-sentence diff test that grader can witness.** Anchor on correctness, never velocity — a task is only as good as the grader that proves it. Write the grader first; the task is built around it.

## INPUT

$ARGUMENTS

- **Spec reference** `NNNNN-FEATURE-<name>` -> load `__FW_SPECS_DIR__/NNNNN-FEATURE-<name>.md` and compile its acceptance criteria into graders.
- **Free text** -> a short description of work; derive the criteria and graders directly.

A spec is a list of acceptance criteria. Each criterion becomes a grader whose passing *is* the proof that criterion holds, and a `goal` that names the behavior change the grader observes. Do not invent criteria the spec does not state. If a criterion has no grader you can express, the task is not yet plannable — surface that, do not paper over it with a weaker check.

## TASK SCHEMA (authoritative)

A task is a JSON object. Two fields carry the load: `goal` and `graders`. Everything else is optional briefing. Match `docs/task-schema.md` exactly — do not add fields it does not define.

| Field           | Required | Meaning                                                                                              |
| --------------- | -------- | ---------------------------------------------------------------------------------------------------- |
| `goal`          | yes      | One sentence naming the observable behavior change. The one-sentence diff test.                       |
| `graders`       | yes      | How "done" is decided. At least one. The spine — write it first. The agent's own claim never overrides it.|
| `id`            | no       | Stable unique identifier (kebab-case, no whitespace). Other tasks reference it via `prerequisites`.   |
| `prerequisites` | no       | Task ids that must reach DONE first. The only ordering mechanism.                                     |
| `tags`          | no       | Free-form labels for filtering and grouping.                                                          |
| `context`       | no       | Briefing the agent reads upfront: `relevant`, `references`, `constraints`, `non_goals`, `edge_cases`, `notes` (all optional). |

Grader types (the harness runs them cost-cheapest-first: `command` -> `transcript` -> `rubric` -> `manual`; a failure aborts the rest):

| Type         | When to use                                              | Required fields                                                     |
| ------------ | ------------------------------------------------------- | ------------------------------------------------------------------- |
| `command`    | The default. Tests, lint, typecheck, build, filesystem state checks — any deterministic shell check. | `run` (non-zero exit = fail)                                        |
| `transcript` | Cap turns / tokens / wall-time on the run itself.       | at least one of `max_turns`, `max_total_tokens`, `max_wall_seconds` |
| `rubric`     | A sparing binary MET/UNMET screen where no runnable oracle exists. Never the authoritative grade. | `assertions[]`                                                      |
| `manual`     | Operator approval gate.                                 | `instruction`                                                       |

There is NO `steps`, `passing`, `acceptance_criteria`, `category`, `commit`, `priority`, or `parallel_group` field. Do not invent fields. Clarifications discovered mid-run belong in the lifecycle record, never edited back into the task — the original definition is immutable.

## THE GRADER LADDER — compile each criterion to the strongest grader

Write the grader before the goal. A criterion you cannot grade is not a task; a grader the agent can satisfy without solving the problem is worse than none. For every criterion, climb this ladder and stop at the strongest rung that fits:

1. **Command grader, preferred.** Express the criterion as a deterministic shell check whose exit code *is* the verdict: a test that fails on the old behavior and passes on the new, a `grep -q` / `test -f` state check, a typecheck, a build. Pass must mean the problem is solved — not that something ran.

2. **Grade end-state, not path.** Never assert a fixed sequence of tool calls, a commit message, or "the function was called." Grade what the produced artifact *does* — run the new behavior and check its result. The agent reaches the end-state however it likes; the grader only cares that it arrived.

3. **Run out-of-band; read-only / hidden test surface.** The harness runs graders outside the agent's reach and trusts no file, output, or status from inside the sandbox — this is the floor, not a nicety (a ten-line `conftest.py` that forces every report to "passed" otherwise resolves everything). Concretely:
   - Point each grader at an **existing, committed** test/lint/build command this repo already trusts, and make it invoke the real entrypoint — not a marker file or a log line the agent could write.
   - Pin the tests the agent must keep green in `context.relevant`, and add a `non_goals` line forbidding edits to the grading surface ("do not modify files under `tests/`; do not weaken or delete assertions"). Access controls beat instruction, so do both: pin the surface *and* forbid touching it.

4. **Composition holdout for shared-invariant criteria.** When a criterion touches an invariant other features also consume (an enum member, a schema column, a required field, a public signature), the dominant cheat is a per-feature handler that passes its own test but never integrates. Add a second `command` grader that exercises the **seam** — the dependents together — not each in isolation. The holdout adds **no new requirement** beyond the spec; it only composes what the visible graders already check. If no seam test exists, the holdout is a `command` that runs the dependents' suites as one.

5. **Optional mutation-strength check.** A test that runs is not a test that discriminates — 100% coverage can have a mutation score of 0. Where cost allows, sanity-check the chosen grader: *would a plausible mutation of this criterion still pass it?* If flipping a boundary, inverting a condition, or returning a constant would slip through, the grader is too weak — strengthen the assertion or widen the input distribution. A strength check, not a mandatory gate; encode the surviving mutation as an `edge_case` so the brief names it.

6. **Rubric as a last-resort binary screen.** Reach for `rubric` only where no runnable oracle exists (a genuine oracle problem — "the error message is human-readable"). Before a model judge, prefer a **metamorphic** command grader: an input->output invariant checked over multiple runs (same input twice gives the same answer; a permuted input gives a permuted output). If you must use `rubric`, keep assertions **binary MET/UNMET**, keep them few, and pair them with at least one `command` grader — the judge is a reward-hack screen layered on top, never the trust-conferring grade. LLM-judge is the flakiest, highest-variance signal; it never stands alone.

**Reliability over capability.** Right-size so a single attempt reliably passes; a task that only passes one run in three is not done in any sense that matters. Prefer deterministic graders and design out async / order / environment nondeterminism, which poisons the authoritative signal.

## RIGHT-SIZE EACH TASK — the sizing test IS the grader test

A task is admitted only if it has an **end-state grader expressible as an observable behavior change.** That single rule collapses right-sizing and grader-strength into one decision: if you cannot write a discriminating grader for a chunk, it is mis-sized — fold it, sharpen it, or make it a prerequisite — before the task is allowed to exist.

| Signal                                       | Result                                                            |
| -------------------------------------------- | ----------------------------------------------------------------- |
| One observable change, one discriminating grader | 1 task                                                        |
| Several cohesive changes behind one outcome  | 1 task with multiple graders                                      |
| Distinct outcomes that share an invariant    | Fold into one task, or split and declare the `prerequisites` edge |
| A layer-only chunk with no observable behavior | Not a task — fold it in or make it a prerequisite of one that is |

- **A layer-only chunk auto-fails as a component.** If the only grader you can write is "the layer exists" or "the migration ran" with no behavior a caller or user can observe, it is a **component, not a task** — fold it into the task that consumes it, or make it a `prerequisite` of one that does. Never ship a layer-only task whose only grader is "the file exists."
- **`goal` is one sentence naming that behavior change.** If the outcome needs "and" to cover two distinct behaviors, split it and chain via `prerequisites`.
- **Size to the largest single-outcome slice the agent reliably completes**, not the smallest fragment. Prefer a vertical slice that delivers one observable change end to end; relax "must touch multiple layers" for a genuinely single-file change. Minimize the files one task touches — cross-file reasoning is the dominant failure axis (a direction, not a LOC cap).
- **Resist micro-tasking.** When two sub-outcomes share an invariant, one cohesive task often beats two wired with an edge: higher single-attempt success, less context, no coordination seam. Split only when the result is genuinely independent and still grader-bearing.
- **Decomposition menu** when a slice is too big to grade as one behavior. Each row forces the split to declare the behavior its grader observes — a legal split is always behavior-bearing, never a horizontal layer:

| Pattern                   | Split rule                                                       | What each task's grader observes      |
| ------------------------- | --------------------------------------------------------------- | ------------------------------------- |
| **Workflow steps**        | Break an end-to-end flow into sequential steps.                 | One step of the flow works end-to-end. |
| **Business-rule variations** | One rule per task; treat each rule as its own slice.         | That rule's behavior, holding others fixed. |
| **Data variations**       | One input shape / format / locale per task.                     | The behavior for that data class.     |
| **CRUD operations**       | Split create / read / update / delete.                          | One operation's observable effect.    |

## ORDER WITH PREREQUISITES ONLY

- **Every ordering signal is an explicit `prerequisites` edge.** No directory order, no `NN-` filename order, no tag order, no "the agent will figure out it needs Y first." Order is derived by topological sort over the edges; a cycle is a planning error.
- **The hard job is edge completeness**, not edge minimization. Keep the graph wide and shallow — many small, loosely-coupled tasks, minimal cross-edges — but declare every real edge. Do **not** split a single true invariant across nodes; that manufactures coupling rather than removing it.
- **Edges are static**, declared here at plan time. The agent never rewrites the graph at runtime. An under-declared edge is caught up front (by enumerating invariant dependents, below) and again at submit time, where the rebase re-runs command graders against the exact base the task lands on.
- **No critical path, float, or slack**, and no per-task duration estimates. The DAG is for correctness and parallel eligibility only; the worker pulls the first eligible task whose prerequisites are DONE.

## ENUMERATE DEPENDENTS OF A SHARED INVARIANT

When a task changes a shape other code asserts against — a new enum member, a schema column, a renamed field, a changed public signature — enumerate **every** dependent up front, before drafting the grader. For each:

- If it is cohesive with this change, **fold it into this task** and pin its test in `context.relevant`.
- Otherwise, **declare a `prerequisites` edge** and name the dependent's test in `context.constraints`, requiring it be updated in the same change.
- Add the **composition holdout** (ladder rung 4) so the seam between producer and dependents is graded, not just each side alone. The holdout adds no requirement beyond the spec — it only composes what the visible graders already check, which is what keeps it from silently becoming scope creep.

Never leave the coupling implicit — the implicit dependency is the under-declared edge that hands the next task a red suite against a stale base.

## WHAT TO PUT IN EACH BRIEF

State outcomes, not procedures — no step-by-step path, no prescribed implementation, no TDD ceremony. The brief gives the smallest set of high-signal tokens:

- **`context.relevant`: pin only files with an unambiguous reason** each — the files to change, plus the tests to keep green. `relevant` is the single biggest lever for cutting context burn. If a human cannot say why a file is here, the agent cannot either.
- **`context.non_goals`: forbid touching the grading surface** (tests, fixtures, the holdout) and anything out of scope — the access-control half of the grader spine.
- **`context.constraints`: require commit-before-done and same-change updates of every shared-invariant dependent.** The worker verifies a committed change against the exact base it lands on, so each task must commit before reporting done.
- **Put the single load-bearing constraint at the start or end** of the brief; keep briefs short enough that nothing important lands in an unread middle.

## STEP 1: UNDERSTAND THE CODEBASE AND ITS GRADER SURFACE

Before drafting anything, get oriented — and specifically find the graders you will reuse. Graders must use this repo's real verification commands, never invented ones.

```bash
ls -la
sed -n '1,100p' README.md 2>/dev/null

# The grader surface: how this repo already proves "done"
sed -n '1,80p' Makefile justfile package.json pyproject.toml 2>/dev/null
ls tests/ test/ spec/ 2>/dev/null

# Existing flywheel state
flywheel status 2>/dev/null
flywheel history --limit 10 2>/dev/null
```

For each criterion, identify: which committed command proves it, which files the change touches, which existing tests pin the surface, and where a holdout for a shared seam would run. If no command can prove a criterion, that is the central planning problem for this task — write a new test target or a metamorphic check before sizing the task around a weaker grader.

## STEP 2: COMPILE CRITERIA INTO TASKS AND GRADERS

For each spec criterion (or each behavior in a free-text request):

1. Write the **grader** first, climbing the ladder to the strongest rung that fits.
2. Write the **`goal`** as the one-sentence behavior change that grader observes.
3. If the grader can only observe a layer with no behavior change, the criterion is a **component** — fold it into its consumer.
4. Enumerate dependents of any shared invariant; declare edges or fold; add the composition holdout.
5. Pin `context.relevant` and fence the grading surface in `context.non_goals`.

For every grader, ask the one universal gate: **does this fail a plausible wrong implementation?** If a `sys.exit(0)`, an always-true stub, or a marker file would pass it, it is no grader at all — strengthen it (assert on real behavior, widen the input distribution) or grade end-state from outside the agent's reach.

### Example task shapes

Minimal — grade end-state with one discriminating committed command:

```json
{
  "id": "add-retry-backoff",
  "goal": "HTTP client retries 5xx and timeout failures with exponential backoff and jitter.",
  "graders": [
    { "type": "command", "run": "npm test -- http", "name": "behavior" }
  ]
}
```

Fully briefed — grader spine first, surface pinned and fenced, a composition holdout over a shared field, and a mutation-aware edge case:

```json
{
  "id": "add-retry-backoff",
  "goal": "HTTP client retries 5xx and timeout failures with exponential backoff and jitter.",
  "prerequisites": ["setup-http-client"],
  "tags": ["http", "reliability"],
  "graders": [
    { "type": "command", "run": "npm test -- http",         "name": "behavior" },
    { "type": "command", "run": "npm test -- integration",  "name": "seam-holdout" },
    { "type": "command", "run": "npm run lint",             "name": "lint" }
  ],
  "context": {
    "relevant": [
      "src/http/client.ts",
      "src/http/config.ts",
      "tests/http/retry.test.ts -- keep this suite green"
    ],
    "references": [
      "src/db/retry.ts -- mirror this backoff structure"
    ],
    "constraints": [
      "Use existing deps; no new packages",
      "ClientConfig gains RetryConfig; update every caller that constructs it (src/http/factory.ts and its test) in this same change",
      "Commit the change with a clear message before reporting done"
    ],
    "non_goals": [
      "Do not modify files under tests/; do not weaken or delete assertions",
      "Do not add a new config file to satisfy the grader"
    ],
    "edge_cases": [
      "Respect Retry-After when the server provides it",
      "Backoff must be bounded -- a returned constant delay must fail the test"
    ]
  }
}
```

Use this repo's actual test/lint/build commands in graders, not the example's. The `seam-holdout` grader composes the dependents of the shared `RetryConfig` field; it adds no requirement beyond the spec.

__FW_DELIVERY__

## RULES

1. **Grader first, every task.** No task ships without an end-state, out-of-band, hard-to-game grader at the strongest ladder rung that fits — one that fails a plausible wrong implementation.
2. **`goal` is one sentence naming an observable behavior change.** A layer-only chunk is a component, not a task.
3. **`prerequisites` is the only ordering mechanism**, and edge completeness is the hard job — enumerate every dependent of a shared invariant.
4. **Pin only justified files; fence the grading surface; commit before verify.** Clarifications go to lifecycle records, never the task.
5. **Rubric/LLM-judge is a sparing binary screen, never the authoritative grade.**
6. **Always present the proposal before writing anything. No emojis. No invented schema fields.**
