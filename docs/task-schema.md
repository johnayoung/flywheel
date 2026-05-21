# Task Schema

A task is the unit of work in flywheel. It describes the desired outcome and how to verify it.

The schema is intentionally minimal: two required fields, the rest optional with sensible defaults. The agent plans its own approach — the task states the goal, not the procedure.

## Required fields

| Field     | Type     | Description                                                   |
| --------- | -------- | ------------------------------------------------------------- |
| `goal`    | string   | Desired outcome. 1-2 sentences. Must pass the one-sentence diff test. |
| `graders` | []Grader | Verification checks; all must pass for the task to be done (min 1). |

## Optional fields

| Field           | Type     | Default   | Description                                |
| --------------- | -------- | --------- | ------------------------------------------ |
| `id`            | string   | `uuid4()` | Unique identifier, no whitespace           |
| `prerequisites` | []string | `[]`      | Task IDs that must complete first          |
| `tags`          | []string | `[]`      | Free-form labels for filtering and grouping |
| `context`       | Context  | (empty)   | Briefing material the agent reads upfront  |

`prerequisites` cannot reference the task's own id. Git workflow concerns (commit messages, branch naming) and loading concerns (files, queues, APIs) live outside the schema.

## Context

A bundle of optional briefing fields. Provide none, one, or all. Use these to give the agent what it can't infer from the code; skip them when not needed.

| Field         | Type     | Description                                                       |
| ------------- | -------- | ----------------------------------------------------------------- |
| `relevant`    | []string | Files, URLs, or doc paths the agent should consult upfront        |
| `references`  | []string | Patterns to mirror or prior implementations to follow             |
| `constraints` | []string | Hard rules the agent must respect (e.g. "don't change the public API") |
| `non_goals`   | []string | Explicitly out-of-scope work                                      |
| `edge_cases`  | []string | Non-obvious cases and known pitfalls                              |
| `notes`       | string   | Free-form markdown for anything that doesn't fit above            |

`relevant` is the single biggest lever for cutting context burn — pointing the agent at the right files saves it from reading the wrong ones.

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

| Field        | Required | Description                                                  |
| ------------ | -------- | ------------------------------------------------------------ |
| `assertions` | yes      | List of natural-language statements; all must be judged true |
| `rubric`     | no       | Path to a markdown file with extended judging guidance       |
| `name`       | no       | Short label                                                  |

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

| Field              | Required | Description                              |
| ------------------ | -------- | ---------------------------------------- |
| `max_turns`        | no       | Maximum agent turns                      |
| `max_total_tokens` | no       | Maximum cumulative tokens across the run |
| `max_wall_seconds` | no       | Maximum wall-clock duration              |

At least one constraint field must be set.

```json
{ "type": "transcript", "max_turns": 20 }
```

## Examples

### Minimum viable

Two fields. `id` is generated; everything else is omitted.

```json
{
  "goal": "Add exponential backoff retry to the HTTP client.",
  "graders": [
    { "type": "command", "run": "uv run pytest tests/http" }
  ]
}
```

### Fully briefed

```json
{
  "id": "add-retry-logic",
  "goal": "HTTP client retries 5xx and timeout failures with exponential backoff and jitter.",
  "prerequisites": ["setup-http-client"],
  "tags": ["http", "reliability"],
  "context": {
    "relevant": [
      "src/flywheel/http/client.py",
      "src/flywheel/http/config.py"
    ],
    "references": [
      "src/flywheel/db/retry.py — mirror this backoff structure"
    ],
    "constraints": [
      "Use stdlib + existing deps; no new packages",
      "Don't change ClientConfig's public surface beyond adding fields"
    ],
    "non_goals": [
      "Don't touch tests outside tests/http",
      "No changes to logging format"
    ],
    "edge_cases": [
      "Respect Retry-After when the server provides it",
      "Cap jitter at base_delay * 2 to avoid pathological backoff"
    ]
  },
  "graders": [
    { "type": "command",    "run": "uv run pytest tests/http",      "name": "tests" },
    { "type": "command",    "run": "uv run ruff check .",           "name": "lint" },
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

## Authoring discipline

- **`goal` is the one-sentence diff test.** If the expected change doesn't fit in one sentence, the task is too large — split along architectural layers (migration, model, service, handler) and chain via `prerequisites`.
- **Prefer `context.relevant` over agent discovery.** Files read is the real context cost.
- **`graders` defines "done."** If you can't write one, you can't size the task.
- **`context.notes` is the escape hatch, not the default.** Reach for it only when nothing structured fits.
- **Don't prescribe procedure.** State the outcome; the agent plans the approach.
