## STEP 3: CHOOSE PHASE DIRECTORY

This repo's flywheel work source is a task directory: one JSON file per task under `__FW_TASKS_DIR__/active/<phase>/`. Phases are just directories; the `NN-` filename prefix controls walk order, and cross-task ordering lives in each task's `prerequisites` field.

```bash
# Existing phases
ls __FW_TASKS_DIR__/active/ __FW_TASKS_DIR__/archive/ 2>/dev/null

# Highest used prefix across both
ls __FW_TASKS_DIR__/active/ __FW_TASKS_DIR__/archive/ 2>/dev/null \
  | grep -E '^[0-9]+-' | sed 's/-.*//' | sort -n | tail -1
```

Decide:
- **Reuse an existing active phase** if the work fits its scope.
- **Create a new phase directory** for distinct chunks of work. Use the next available `NN-` prefix and a kebab-case slug, e.g. `03-http-reliability`.

## STEP 4: DRAFT TASK JSON

Every task is its own JSON file. The filename is the task `id` with `.json` (e.g. `add-retry-backoff.json` -> `id: "add-retry-backoff"`). IDs must be unique repo-wide and stable (other tasks may reference them via `prerequisites`).

## STEP 5: PRESENT THE PROPOSAL

Do not write anything yet. Show the proposed phase directory + task IDs + each full JSON for review:

```
## Proposal

**Phase directory:** `__FW_TASKS_DIR__/active/03-http-reliability/`
  - new directory (no existing phase fits)
  - OR: existing directory `__FW_TASKS_DIR__/active/02-foo/` (work fits this scope)

### Tasks

1. **setup-http-client** (no prereqs)
   - Goal: <one sentence>
   - Graders: command(tests)

2. **add-retry-backoff** (requires: setup-http-client)
   - Goal: <one sentence>
   - Graders: command(tests), command(lint)

### Full JSON

<full JSON for each task>

---

**Proceed?** Reply to confirm and I'll write the files.
```

## STEP 6: WRITE FILES (after confirmation)

1. `mkdir -p __FW_TASKS_DIR__/active/<phase-dir>/`
2. For each task, write `<phase-dir>/<task-id>.json` (one task per file).
3. Validate: run `flywheel status` -- it loads every active task file and errors on an invalid one. Each file must also parse as JSON (`python3 -m json.tool <file>` works in a pinch).
4. Report the written paths and the order the worker will pick them up. The operator runs them with `flywheel worker` (or `flywheel worker --once` for a single drain) and watches with `flywheel status` / `fw`.
