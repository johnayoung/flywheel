# Task Schema

A task is the unit of work in flywheel. It describes what the agent should do and how to verify it did it.

## Fields

| Field           | Type      | Required | Description                                                |
| --------------- | --------- | -------- | ---------------------------------------------------------- |
| `id`            | string    | yes      | Unique identifier, no whitespace                           |
| `description`   | string    | yes      | What this task accomplishes                                |
| `category`      | string    | yes      | One of: `feat`, `fix`, `refactor`, `test`, `docs`, `chore` |
| `priority`      | int       | no       | Ordering hint for schedulers                               |
| `prerequisites` | []string  | no       | Task IDs that must complete first (cannot reference self)  |
| `steps`         | []string  | yes      | Ordered instructions for the agent (min 1)                 |
| `graders`       | []Grader  | yes      | Verification checks; all must pass for the task to be done (min 1) |

Git workflow concerns (commit messages, branch naming) are owned by the [Strategy](strategy.md) layer, not the task. How tasks are loaded (files, queues, APIs) is a concern of the caller, not the schema.

## Graders

Each entry in `graders` is a typed object. The `type` field selects which other fields apply. A task is `done` only when every grader passes. Graders are binary — no partial credit, no weights.

The harness runs graders cost-cheapest-first: `command` → `transcript` → `rubric` → `manual`. Within a type, list order is respected. A failure inside one type aborts the rest of that type and skips later (more expensive) types.

### `command` — deterministic shell check

The general workhorse. Tests, lint, typecheck, build, custom scripts, filesystem state checks (`test -f ...`, `grep -q ...`). Pass = exit 0.

| Field  | Required | Description                         |
| ------ | -------- | ----------------------------------- |
| `run`  | yes      | Shell command to execute            |
| `name` | no       | Short label for logs/UI attribution |

```json
{ "type": "command", "run": "uv run pytest tests/http", "name": "tests" }
```

### `rubric` — LLM-judged semantic check

Natural-language assertions evaluated by a separate LLM call against the goal, diff, and execution artifacts. Use for intent-level checks that deterministic commands cannot express.

| Field        | Required | Description                                                       |
| ------------ | -------- | ----------------------------------------------------------------- |
| `assertions` | yes      | List of natural-language statements; all must be judged true      |
| `rubric`     | no       | Path to a markdown file with extended judging guidance            |
| `name`       | no       | Short label                                                       |

```json
{
  "type": "rubric",
  "assertions": [
    "Retries on 5xx and timeout errors only",
    "Respects max_retries config (default 3)"
  ]
}
```

### `manual` — human approval

Pauses the loop and surfaces an instruction for an operator. Pass = operator approves.

| Field         | Required | Description                                |
| ------------- | -------- | ------------------------------------------ |
| `instruction` | yes      | What the operator should review and decide |
| `name`        | no       | Short label                                |

```json
{ "type": "manual", "instruction": "Confirm jitter algorithm fits upstream rate limits" }
```

### `transcript` — path-level constraint

Asserts against the run record rather than the produced outcome. The harness also enforces these as hard limits during execution, not only at grade time — `max_turns` will abort a runaway loop before the grader stage is reached.

| Field              | Required | Description                                       |
| ------------------ | -------- | ------------------------------------------------- |
| `max_turns`        | no       | Maximum agent turns                               |
| `max_total_tokens` | no       | Maximum cumulative tokens across the run          |
| `max_wall_seconds` | no       | Maximum wall-clock duration                       |

At least one constraint field must be set.

```json
{ "type": "transcript", "max_turns": 20 }
```

## Example

```json
{
  "id": "add-retry-logic",
  "description": "Add exponential backoff retry to HTTP client",
  "category": "feat",
  "priority": 1,
  "prerequisites": ["setup-http-client"],
  "steps": [
    "Add retry middleware to flywheel/http/client.py",
    "Implement exponential backoff with jitter",
    "Add max_retries and base_delay to ClientConfig"
  ],
  "graders": [
    { "type": "command",    "run": "uv run pytest tests/http", "name": "tests" },
    { "type": "command",    "run": "uv run ruff check .",       "name": "lint" },
    { "type": "command",    "run": "uv run mypy src/flywheel/http", "name": "typecheck" },
    { "type": "rubric",     "assertions": [
        "Retries on 5xx and timeout errors only",
        "Respects max_retries config (default 3)"
    ]},
    { "type": "transcript", "max_turns": 20 },
    { "type": "manual",     "instruction": "Confirm jitter algorithm fits upstream rate limits" }
  ]
}
```
