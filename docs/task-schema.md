# Task Schema

A task is the unit of work in flywheel. It describes what the agent should do and how to verify it did it.

## Fields

| Field                 | Type     | Required | Description                                                     |
| --------------------- | -------- | -------- | --------------------------------------------------------------- |
| `id`                  | string   | yes      | Unique identifier, no whitespace                                |
| `description`         | string   | yes      | What this task accomplishes                                     |
| `category`            | string   | yes      | One of: `feat`, `fix`, `refactor`, `test`, `docs`, `chore`      |
| `priority`            | int      | no       | Ordering hint for schedulers                                    |
| `prerequisites`       | []string | no       | Task IDs that must complete first (cannot reference self)       |
| `steps`               | []string | yes      | Ordered instructions for the agent (min 1)                      |
| `acceptance_criteria` | object   | yes      | What "done" means — must have at least one command or condition |

Git workflow concerns (commit messages, branch naming) are owned by the [Strategy](strategy.md) layer, not the task.

## Input formats

Tasks can be loaded from:

- **Single JSON file** — one task object per file
- **Directory of JSON files** — each `.json` file in the directory is one task
- **JSONL** — one task JSON object per line

All formats validate on parse. Invalid tasks fail loudly with the specific constraint that was violated.

## Acceptance criteria

The `acceptance_criteria` object has two keys:

| Key          | Type     | Description                                                          |
| ------------ | -------- | -------------------------------------------------------------------- |
| `commands`   | []string | Deterministic checks the harness executes. Pass/fail, no LLM needed. |
| `conditions` | []string | Semantic criteria requiring judgment (LLM-as-judge or human).        |

At least one command or condition must be present. Both may be present.

`commands` run first during verification. If any command fails, the agent gets the failure output and loops back — expensive checks are skipped. `conditions` are evaluated only after all commands pass.

## Example

```json
{
  "id": "add-retry-logic",
  "description": "Add exponential backoff retry to HTTP client",
  "category": "feat",
  "priority": 1,
  "prerequisites": ["setup-http-client"],
  "steps": [
    "Add retry middleware to internal/http/client.go",
    "Implement exponential backoff with jitter",
    "Add max_retries and base_delay to ClientConfig"
  ],
  "acceptance_criteria": {
    "commands": [
      "go test ./internal/http/...",
      "go vet ./..."
    ],
    "conditions": [
      "Retries on 5xx and timeout errors only",
      "Respects max_retries config (default 3)"
    ]
  }
}
```
