## STEP 3: WORK ITEMS ARE GITHUB ISSUES

This repo's flywheel work source is GitHub Issues: the worker picks up open issues in `__FW_GH_REPO__` labeled `__FW_GH_LABEL__`. One issue per task.

How an issue compiles to a task:
- `id` is `gh-<number>` (stable; reference it from other issues' `prerequisites`).
- `goal` is the issue title, unless the spec block overrides it.
- The issue body MAY embed a fenced spec block whose info string is exactly `flywheel`, carrying any of `goal`, `graders`, `context`, `tags`, `prerequisites`. An issue with no graders (and no policy default graders) is skipped as not runnable, so always include graders in the spec block.
- The rest of the issue body reaches the agent as `context.notes`.

## STEP 4: DRAFT THE ISSUES

For each task, draft a title (the goal) and a body containing the spec block:

````markdown
<one-paragraph human framing of the work>

```flywheel
{
  "goal": "HTTP client retries 5xx and timeout failures with exponential backoff and jitter.",
  "prerequisites": ["gh-12"],
  "graders": [
    { "type": "command", "run": "npm test -- http", "name": "tests" }
  ],
  "context": {
    "relevant": ["src/http/client.ts"],
    "constraints": ["Commit the change with a clear message before reporting done"]
  }
}
```
````

## STEP 5: PRESENT THE PROPOSAL

Do not create anything yet. Show each proposed issue (title, prerequisites, full body including the spec block) for review, then ask to proceed.

## STEP 6: CREATE ISSUES (after confirmation)

For each task:

```bash
gh issue create \
  --repo "__FW_GH_REPO__" \
  --title "<goal>" \
  --label "__FW_GH_LABEL__" \
  --body-file <drafted-body.md>
```

Note each created issue number. If a later task's `prerequisites` references an earlier one, create them in dependency order and substitute the real `gh-<number>` ids before creating the dependents.

Report the created issue URLs. The operator runs them with `flywheel worker` and watches with `flywheel status` / `fw`; outcomes are posted back to each issue as a comment.
