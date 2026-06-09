---
description: Turn a request or spec into one or more flywheel task JSON files
---

Generate flywheel-schema task definitions (per `docs/task-schema.md`) and drop one JSON file per task into a phase directory under `.flywheel/tasks/active/`. The worker (`task-worker.sh`) picks these up and runs them via `python -m flywheel.workflow run`.

## INPUT

$ARGUMENTS

Input formats:
- **Spec reference**: `NNNNN-FEATURE-<name>` -> load `.flywheel/specs/NNNNN-FEATURE-<name>.md` and derive tasks from it
- **Free text**: short description of work -> generate tasks directly

## CORE PRINCIPLES

- Tasks conform to `docs/task-schema.md`. That doc is authoritative. Re-read it before writing.
- Two required fields: `goal` and `graders`. Everything else is optional with sensible defaults.
- **`goal` is the one-sentence diff test.** If the change does not fit in one sentence, split the task and chain via `prerequisites`.
- **`graders` defines "done."** If you cannot write at least one grader, you cannot size the task.
- **Do not prescribe procedure.** Tasks state the outcome; the agent plans the approach. Do NOT include a `steps[]` field — it does not exist in the schema.
- **`context.relevant` is the single biggest lever for cutting context burn.** Point the agent at the right files instead of making it discover them.

## STEP 1: UNDERSTAND THE CODEBASE

Before generating tasks, get oriented:

```bash
ls -la
cat README.md 2>/dev/null | head -100
ls docs/ 2>/dev/null

# Existing phases and tasks
ls .flywheel/tasks/active/ 2>/dev/null
ls .flywheel/tasks/archive/ 2>/dev/null
```

Identify which files the work touches, which patterns it should mirror, and which existing tests cover the surface so graders can run them.

## STEP 2: ASSESS COMPLEXITY

| Complexity | Indicators                                        | Result                   |
| ---------- | ------------------------------------------------- | ------------------------ |
| Simple     | Single change with one obvious grader             | 1 task                   |
| Medium     | Multiple cohesive changes, one architectural slice | 2-4 tasks, one phase     |
| Complex    | Cross-cutting / multiple architectural layers     | New phase, chained tasks |

Splitting heuristic from `docs/task-schema.md`: split along architectural layers (migration, model, service, handler) and chain with `prerequisites`.

## STEP 3: CHOOSE PHASE DIRECTORY

Phases are just directories. Filename prefix (`NN-...`) controls walk order. The worker has no notion of phase metadata — cross-task ordering lives in each task's `prerequisites` field.

```bash
# Existing phases
ls .flywheel/tasks/active/ .flywheel/tasks/archive/ 2>/dev/null

# Highest used prefix across both
ls .flywheel/tasks/active/ .flywheel/tasks/archive/ 2>/dev/null \
  | grep -E '^[0-9]+-' | sed 's/-.*//' | sort -n | tail -1
```

Decide:
- **Reuse an existing active phase** if the work fits its scope.
- **Create a new phase directory** for distinct chunks of work. Use the next available `NN-` prefix and a kebab-case slug, e.g. `03-dogfood-workflow`.

## STEP 4: DRAFT TASK JSON

Every task is its own JSON file. The filename should be the task `id` with `.json` (e.g. `dogfood-cli.json` -> `id: "dogfood-cli"`). IDs must be unique repo-wide, contain no whitespace, and be stable (other tasks may reference them via `prerequisites`).

### Minimal viable task

```json
{
  "id": "add-retry-backoff",
  "goal": "HTTP client retries 5xx and timeout failures with exponential backoff and jitter.",
  "graders": [
    { "type": "command", "run": "uv run pytest tests/http" }
  ]
}
```

### Fully briefed task

```json
{
  "id": "add-retry-backoff",
  "goal": "HTTP client retries 5xx and timeout failures with exponential backoff and jitter.",
  "prerequisites": ["setup-http-client"],
  "tags": ["http", "reliability"],
  "context": {
    "relevant": [
      "src/flywheel/http/client.py",
      "src/flywheel/http/config.py"
    ],
    "references": [
      "src/flywheel/db/retry.py -- mirror this backoff structure"
    ],
    "constraints": [
      "Use stdlib + existing deps; no new packages",
      "Commit the change with a Conventional Commits message before reporting verify"
    ],
    "non_goals": [
      "Don't touch tests outside tests/http"
    ],
    "edge_cases": [
      "Respect Retry-After when the server provides it"
    ]
  },
  "graders": [
    { "type": "command", "run": "uv run pytest tests/http", "name": "tests" },
    { "type": "command", "run": "uv run ruff check .",      "name": "lint" }
  ]
}
```

### Grader types (see `docs/task-schema.md` for full spec)

| Type         | When to use                                       | Required fields                                  |
| ------------ | ------------------------------------------------- | ------------------------------------------------ |
| `command`    | Tests, lint, typecheck, filesystem state checks   | `run`                                            |
| `rubric`     | LLM-judged intent (use sparingly, expensive)      | `assertions[]`                                   |
| `manual`     | Operator approval needed                          | `instruction`                                    |
| `transcript` | Cap turns / tokens / wall-time on the run itself  | at least one of `max_turns`, `max_total_tokens`, `max_wall_seconds` |

Prefer `command` graders. A grader fails on non-zero exit; that is "done" for it.

### Authoring discipline

- **Commits are the agent's job.** Flywheel does not own git. Add a constraint to `context.constraints` telling the agent to commit (with a Conventional Commits message) before emitting `intent=verify`. Verify the commit landed with a `command` grader like `git log -1 --format=%s | grep -q '^feat'` if needed.
- **No `passing`, `steps`, `acceptance_criteria`, `category`, `commit`, `parallel_group`, `priority`, `github_item_id` fields.** Those came from the old schema and are gone. The schema enforces a tight surface — extra fields are silently ignored on load, but they pollute the file and confuse readers.
- **`prerequisites` is the only ordering mechanism.** Files within a phase run alphabetically when no prerequisite forces order.
- **Enumerate the dependents of a shared invariant.** When a task adds a new enum value, a new schema column, a new required field, or otherwise changes a shape that other tests/graders assert against, list every test and grader that pins down the old shape in `context.constraints` and require the agent to update them in the same commit. Otherwise the next task in the phase inherits a red suite and is forced to either block or ship `verify` against a known-failing grader (see `.flywheel/audits/02-harness-resilience.md`).

### Loop-path features: emit an in-loop-verification slot

Spec `.flywheel/specs/00017-FEATURE-in-loop-verification-gate.md` defines a mechanical archive gate (`archive_completed_phases`) that refuses to archive a phase whose cumulative diff trips any watched signal unless the phase contains a DONE task **tagged `in-loop-verification`** (or a recorded opt-out artifact). When the spec or the phase diff falls in the **Trigger Set**, the proposal MUST include one such slot.

**Trigger Set (from the spec's Behavior Specification — emit the slot if the work plausibly produces any of these):**

| # | Signal | Decidable test |
| - | ------ | -------------- |
| 1 | New `Status` / `Outcome` member or transition rule | added enum member / `_VALID_EDGES`-style entry in `lifecycle.py` |
| 2 | New `ADD COLUMN` / table in `_schema/*.sql` the live store binds to | new column/table DDL (lifecycles, grader_results, events, attempts, control_commands) |
| 3 | New `Grader` union variant | new variant in `task.py`'s `Grader` union or new `grader_*.py` module the harness dispatches |
| 4 | New `store_protocols.py` Protocol method + dispatch registration | new `def` on a Protocol class + a reactive-sweep / transition-dispatch entry point |
| 5 | New control-command verb | new `CONTROL_COMMAND_*` constant in `invoker_client.py` |

If unsure, emit the slot — the gate's marker is an over-approximation; downgrade later via the opt-out artifact, not by silently skipping.

**Slot template (do NOT auto-generate the fixture body — author writes the test):**

```json
{
  "id": "in-loop-verify-<feature>",
  "goal": "Drive a fixture through the real orchestrate loop with a scripted invoker so the new <Status/column/grader/Protocol method/control verb> is produced AND applied end-to-end by the real loop, and (for schema features) seeded from a v(N-1) store migrated forward via the real SqliteStore.",
  "prerequisites": ["<every feature task id in this phase>"],
  "tags": ["in-loop-verification"],
  "context": {
    "relevant": [
      "tests/test_worker.py::test_run_once_merges_completed_task",
      "src/flywheel/orchestrator.py",
      "src/flywheel/harness.py",
      "src/flywheel/store_sqlite.py",
      "src/flywheel/invoker_client.py"
    ],
    "references": [
      ".flywheel/specs/00017-FEATURE-in-loop-verification-gate.md FR-3 (both loop-produced ends) and FR-4 (v(N-1) seed + real forward migration + SqliteStore round-trip)",
      "tests/test_worker.py::test_run_once_merges_completed_task -- model for driving a full real-loop cycle with a scripted invoke against a real store"
    ],
    "constraints": [
      "FR-3: drive BOTH loop-produced ends -- the harness park AND the reactive-sweep apply. Stubbing either end fails the slot.",
      "FR-4 (schema-touching features): seed a v(N-1) store fixture, run the real forward migration in store_sqlite.py to vN, then assert the new column/table round-trips through SqliteStore (e.g. load_lifecycle). A fresh current-schema-bootstrapped store does NOT satisfy this -- it reproduces phase 08's blind spot.",
      "Use the existing injectable invoker seam fed a deterministic scripted envelope (the same seam tests/test_worker.py uses). Never call a paid/network agent.",
      "Never read, write, or migrate .flywheel/flywheel.sqlite -- fixture/temp stores only.",
      "Tag MUST include `in-loop-verification` verbatim -- archive_completed_phases keys off this exact tag to recognize the verify task.",
      "Commit with a Conventional Commits message (test: ... or feat: ...) before emitting intent=verify."
    ],
    "non_goals": [
      "Do not stub orchestrate, harness, or SqliteStore -- only the agent's text is scripted.",
      "Do not auto-generate the fixture or assertions; the human authoring the slot writes the test body."
    ]
  },
  "graders": [
    { "type": "command", "run": "uv run pytest tests/test_<feature>_in_loop.py", "name": "in-loop-verify" }
  ]
}
```

Keep the slot's `goal` to one sentence, swap the bracketed signal name to whichever the feature actually adds, and list every feature task as a prerequisite so the verify slot runs last. Do NOT auto-fill the fixture body or the assertion text -- spec Out of Scope item: "Auto-generating the verify task's fixture/assertions content (authoring discipline remains human; only the task slot is auto-required)."

## STEP 5: PRESENT THE PROPOSAL

Do not write anything yet. Show the proposed phase directory + task IDs + each full JSON for review:

```
## Proposal

**Phase directory:** `.flywheel/tasks/active/03-dogfood-workflow/`
  - new directory (no existing phase fits)
  - OR: existing directory `.flywheel/tasks/active/02-foo/` (work fits this scope)

### Tasks

1. **dogfood-cli** (no prereqs)
   - Goal: <one sentence>
   - Graders: command(pytest)

2. **dogfood-worker** (requires: dogfood-cli)
   - Goal: <one sentence>
   - Graders: command(bash -n task-worker.sh)

### Full JSON

<full JSON for each task>

---

**Proceed?** Reply to confirm and I'll write the files.
```

## STEP 6: WRITE FILES (after confirmation)

1. `mkdir -p .flywheel/tasks/active/<phase-dir>/`
2. For each task, write `<phase-dir>/<task-id>.json` (one task per file).
3. Validate each file loads cleanly:
   ```bash
   uv run python -c "from flywheel.loaders import load_task_file; \
     import sys; [load_task_file(p) for p in sys.argv[1:]]" \
     .flywheel/tasks/active/<phase-dir>/*.json
   ```
4. Report the written paths and the order the worker will pick them up.

## RULES

1. **Conform to `docs/task-schema.md`** -- it overrides this command if they disagree.
2. **One task per file** -- the worker iterates files, not arrays.
3. **`goal` is one sentence** -- if longer, split the task.
4. **At least one grader** -- usually a `command` grader running tests or lint.
5. **Always present the proposal before writing.**
6. **No emojis. No `steps[]`. No `passing`. No `commit` field.**
