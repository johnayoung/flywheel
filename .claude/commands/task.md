---
description: Turn a request or spec into one or more flywheel task JSON files
---

Generate flywheel-schema task definitions (per `docs/task-schema.md`) and drop one JSON file per task into a phase directory under `.workflow/tasks/active/`. The worker (`task-worker.sh`) picks these up and runs them via `python -m flywheel.workflow run`.

## INPUT

$ARGUMENTS

Input formats:
- **Spec reference**: `NNNNN-FEATURE-<name>` -> load `.workflow/specs/NNNNN-FEATURE-<name>.md` and derive tasks from it
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
ls .workflow/tasks/active/ 2>/dev/null
ls .workflow/tasks/archive/ 2>/dev/null
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
ls .workflow/tasks/active/ .workflow/tasks/archive/ 2>/dev/null

# Highest used prefix across both
ls .workflow/tasks/active/ .workflow/tasks/archive/ 2>/dev/null \
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

## STEP 5: PRESENT THE PROPOSAL

Do not write anything yet. Show the proposed phase directory + task IDs + each full JSON for review:

```
## Proposal

**Phase directory:** `.workflow/tasks/active/03-dogfood-workflow/`
  - new directory (no existing phase fits)
  - OR: existing directory `.workflow/tasks/active/02-foo/` (work fits this scope)

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

1. `mkdir -p .workflow/tasks/active/<phase-dir>/`
2. For each task, write `<phase-dir>/<task-id>.json` (one task per file).
3. Validate each file loads cleanly:
   ```bash
   uv run python -c "from flywheel.loaders import load_task_file; \
     import sys; [load_task_file(p) for p in sys.argv[1:]]" \
     .workflow/tasks/active/<phase-dir>/*.json
   ```
4. Report the written paths and the order the worker will pick them up.

## RULES

1. **Conform to `docs/task-schema.md`** -- it overrides this command if they disagree.
2. **One task per file** -- the worker iterates files, not arrays.
3. **`goal` is one sentence** -- if longer, split the task.
4. **At least one grader** -- usually a `command` grader running tests or lint.
5. **Always present the proposal before writing.**
6. **No emojis. No `steps[]`. No `passing`. No `commit` field.**
