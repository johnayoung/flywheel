---
name: fw-plan
description: Turn a request or fw-spec specification into flywheel tasks the worker can execute
argument-hint: <NNNNN-FEATURE-name or free-text description>
---
<!-- managed-by: flywheel init -->

Generate flywheel task definitions and hand them to the worker. A flywheel task states an outcome (`goal`) and how "done" is decided (`graders`); the agent plans its own approach. The worker (`flywheel worker`) picks tasks up and drives each one through the verification loop.

## INPUT

$ARGUMENTS

Input formats:
- **Spec reference**: `NNNNN-FEATURE-<name>` -> load `__FW_SPECS_DIR__/NNNNN-FEATURE-<name>.md` and derive tasks from it
- **Free text**: short description of work -> generate tasks directly

## TASK SCHEMA (authoritative)

A task is a JSON object. Two required fields: `goal` and `graders`. Everything else is optional.

| Field           | Required | Meaning                                                              |
| --------------- | -------- | -------------------------------------------------------------------- |
| `id`            | no       | Stable unique identifier (kebab-case, no whitespace). Other tasks reference it via `prerequisites`. |
| `goal`          | yes      | One sentence describing the outcome. The one-sentence diff test.      |
| `graders`       | yes      | How "done" is decided. At least one.                                  |
| `prerequisites` | no       | Task ids that must reach DONE first. The only ordering mechanism.     |
| `tags`          | no       | Free-form labels.                                                     |
| `context`       | no       | Briefing for the agent: `relevant`, `references`, `constraints`, `non_goals`, `edge_cases`, `notes` (all optional lists/strings). |

Grader types:

| Type         | When to use                                       | Required fields                                  |
| ------------ | ------------------------------------------------- | ------------------------------------------------ |
| `command`    | Tests, lint, typecheck, filesystem state checks   | `run` (non-zero exit = fail)                     |
| `rubric`     | LLM-judged intent (use sparingly, expensive)      | `assertions[]`                                   |
| `manual`     | Operator approval needed                          | `instruction`                                    |
| `transcript` | Cap turns / tokens / wall-time on the run itself  | at least one of `max_turns`, `max_total_tokens`, `max_wall_seconds` |

Prefer `command` graders.

There is NO `steps`, `passing`, `acceptance_criteria`, `category`, `commit`, `parallel_group`, or `priority` field. Do not invent fields.

## CORE PRINCIPLES

- **`goal` is the one-sentence diff test.** If the change does not fit in one sentence, split the task and chain via `prerequisites`.
- **`graders` defines "done."** If you cannot write at least one grader, you cannot size the task.
- **Do not prescribe procedure.** Tasks state the outcome; the agent plans the approach.
- **`context.relevant` is the single biggest lever for cutting context burn.** Point the agent at the right files instead of making it discover them.
- **Commits are the agent's job.** Flywheel does not own git. Add a constraint telling the agent to commit before reporting done, and verify with a `command` grader when it matters.
- **Enumerate the dependents of a shared invariant.** When a task changes a shape other tests assert against (a new enum member, schema column, required field), list every test and grader that pins the old shape in `context.constraints` and require updating them in the same commit. Otherwise the next task inherits a red suite.

## STEP 1: UNDERSTAND THE CODEBASE

Before generating tasks, get oriented:

```bash
ls -la
cat README.md 2>/dev/null | head -100

# Existing flywheel state
flywheel status 2>/dev/null
flywheel history --limit 10 2>/dev/null
```

Identify which files the work touches, which patterns it should mirror, and which existing tests cover the surface so graders can run them.

## STEP 2: ASSESS COMPLEXITY

| Complexity | Indicators                                         | Result                   |
| ---------- | -------------------------------------------------- | ------------------------ |
| Simple     | Single change with one obvious grader              | 1 task                   |
| Medium     | Multiple cohesive changes, one architectural slice | 2-4 chained tasks        |
| Complex    | Cross-cutting / multiple architectural layers      | Several chained tasks    |

Splitting heuristic: split along architectural layers (migration, model, service, handler) and chain with `prerequisites`.

### Example task shapes

Minimal viable task:

```json
{
  "id": "add-retry-backoff",
  "goal": "HTTP client retries 5xx and timeout failures with exponential backoff and jitter.",
  "graders": [
    { "type": "command", "run": "npm test -- http" }
  ]
}
```

Fully briefed task:

```json
{
  "id": "add-retry-backoff",
  "goal": "HTTP client retries 5xx and timeout failures with exponential backoff and jitter.",
  "prerequisites": ["setup-http-client"],
  "tags": ["http", "reliability"],
  "context": {
    "relevant": [
      "src/http/client.ts",
      "src/http/config.ts"
    ],
    "references": [
      "src/db/retry.ts -- mirror this backoff structure"
    ],
    "constraints": [
      "Use existing deps; no new packages",
      "Commit the change with a clear message before reporting done"
    ],
    "non_goals": [
      "Don't touch tests outside tests/http"
    ],
    "edge_cases": [
      "Respect Retry-After when the server provides it"
    ]
  },
  "graders": [
    { "type": "command", "run": "npm test -- http", "name": "tests" },
    { "type": "command", "run": "npm run lint",     "name": "lint" }
  ]
}
```

Use this repo's actual test/lint commands in graders, not the example's.

__FW_DELIVERY__

## RULES

1. **One sentence per `goal`** -- if longer, split the task.
2. **At least one grader** -- usually a `command` grader running tests or lint.
3. **`prerequisites` is the only ordering mechanism.**
4. **Always present the proposal before writing anything.**
5. **No emojis. No invented schema fields.**
