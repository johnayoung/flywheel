## STEP 3: CHOOSE PHASE DIRECTORY

This repo's work source is a task directory: one JSON file per task under `__FW_TASKS_DIR__/active/<phase>/`. Phases are just directories; the `NN-` filename prefix controls walk order only — it is **not** an ordering mechanism between tasks. Cross-task order lives solely in each task's `prerequisites` field.

```bash
# Existing phases
ls __FW_TASKS_DIR__/active/ __FW_TASKS_DIR__/archive/ 2>/dev/null

# Highest used prefix across both
ls __FW_TASKS_DIR__/active/ __FW_TASKS_DIR__/archive/ 2>/dev/null \
  | grep -E '^[0-9]+-' | sed 's/-.*//' | sort -n | tail -1
```

Decide:
- **Reuse an existing active phase** if the work fits its scope.
- **Create a new phase directory** for a distinct chunk of work — next available `NN-` prefix, kebab-case slug (e.g. `03-http-reliability`).

## STEP 4: DRAFT TASK JSON

One JSON file per task. The filename is the task `id` plus `.json` (`add-retry-backoff.json` -> `id: "add-retry-backoff"`). IDs are unique repo-wide and stable — other tasks reference them via `prerequisites`. Keep the grader array first in each file: it is the spine the reviewer reads first. Each `run` string uses this repo's real verification commands and grades the committed end-state from outside the agent's turn.

## STEP 5: PRESENT THE PROPOSAL

Do not write anything yet. Show the proposed phase directory, the task IDs with their prerequisite edges (so the topological order and the wide-and-shallow shape are reviewable), and for each task its goal, its named graders (which ladder rung each reached), and the full JSON:

```
## Proposal

**Phase directory:** `__FW_TASKS_DIR__/active/03-http-reliability/`
  - new directory (no existing phase fits)
  - OR: existing `__FW_TASKS_DIR__/active/02-foo/` (work fits this scope)

### Tasks (topological order over prerequisites)

1. **setup-http-client** (no prereqs)
   - Goal: <one sentence, observable change>
   - Graders: command(behavior) -- fails a no-op client
   - Grader spine: end-state command; surface pinned and fenced in non_goals

2. **add-retry-backoff** (requires: setup-http-client)
   - Goal: <one sentence, observable change>
   - Graders: command(behavior), command(seam-holdout), command(lint)
   - Grader spine: behavior command + composition holdout over shared RetryConfig
   - Shared invariant: ClientConfig gains RetryConfig; factory.ts + its test updated in this task

### Full JSON

<full JSON for each task, graders first>

---

**Proceed?** Reply to confirm and I'll write the files.
```

## STEP 6: WRITE FILES (after confirmation)

1. `mkdir -p __FW_TASKS_DIR__/active/<phase-dir>/`
2. Write `<phase-dir>/<task-id>.json` for each task (one task per file).
3. Validate: `flywheel status` loads every active task file and errors on an invalid one — it also surfaces any prerequisite cycle or dangling edge. Each file must also parse as JSON (`python3 -m json.tool <file>` in a pinch).
4. Report the written paths and the order the worker will pick them up (the topological order over `prerequisites`). The operator runs them with `flywheel worker` (or `flywheel worker --once` for a single drain) and watches with `flywheel status` / `fw`.
