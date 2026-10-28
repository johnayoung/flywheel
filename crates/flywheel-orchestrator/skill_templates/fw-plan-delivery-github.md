## STEP 3: WORK ITEMS ARE GITHUB ISSUES

This repo's work source is GitHub Issues: the worker picks up open issues in `__FW_GH_REPO__` labeled `__FW_GH_LABEL__`. One issue per task. The task shape does not change with the sink — the same right-sized `goal`, discriminating graders, and `prerequisites` edges you would write for a task file go into a spec block in the issue body. Only where it lands changes.

How an issue compiles to a task:
- `id` is `gh-<number>` (stable; reference it from other issues' `prerequisites`).
- `goal` is the issue title, unless the spec block overrides it.
- The issue body MAY embed a fenced spec block whose info string is exactly `flywheel`, carrying any of `goal`, `graders`, `context`, `tags`, `prerequisites`. An issue with **no graders** (and no policy default graders) is skipped as not runnable — so the grader spine is non-optional here: always include graders in the spec block.
- The rest of the body reaches the agent as `context.notes`.

## STEP 4: DRAFT THE ISSUES

For each task, draft a title (the one-sentence `goal`, an observable behavior change) and a body containing the spec block. Put the **graders first** in the block — they are the spine the reviewer reads first. Encode the ladder you climbed: an end-state `command` grader, a `command` composition holdout for any shared invariant, the tests to keep green pinned, and a `non_goals` line fencing the grading surface. Graders use this repo's real verification commands and grade the committed end-state from outside the agent's turn.

````markdown
<one-paragraph human framing of the work>

```flywheel
{
  "goal": "HTTP client retries 5xx and timeout failures with exponential backoff and jitter.",
  "prerequisites": ["gh-12"],
  "graders": [
    { "type": "command", "run": "npm test -- http",        "name": "behavior" },
    { "type": "command", "run": "npm test -- integration", "name": "seam-holdout" },
    { "type": "command", "run": "npm run lint",            "name": "lint" }
  ],
  "context": {
    "relevant": ["src/http/client.ts", "tests/http/retry.test.ts -- keep this suite green"],
    "constraints": [
      "ClientConfig gains RetryConfig; update every caller (src/http/factory.ts and its test) in this same change",
      "Commit the change with a clear message before reporting done"
    ],
    "non_goals": ["Do not modify files under tests/; do not weaken or delete assertions"],
    "edge_cases": ["Backoff must be bounded -- a returned constant delay must fail the test"]
  }
}
```
````

The `seam-holdout` grader composes the dependents of the shared `RetryConfig` field; it adds no requirement beyond the spec. Use this repo's actual test/lint/build commands, not the example's.

## STEP 5: PRESENT THE PROPOSAL

Do not create anything yet. Show each proposed issue — title, prerequisite edges (so the topological order and wide-and-shallow shape are reviewable), and full body including the spec block — and for each task call out the grader spine (which ladder rung each grader reached, what the holdout composes) and that every dependent of a shared invariant is either folded in or carries an edge. Then ask to proceed.

## STEP 6: CREATE ISSUES (after confirmation)

Create issues in dependency (topological) order so a dependent can reference the real `gh-<number>` of its prerequisite:

```bash
gh issue create \
  --repo "__FW_GH_REPO__" \
  --title "<goal>" \
  --label "__FW_GH_LABEL__" \
  --body-file <drafted-body.md>
```

Note each created issue number. Where a later task's `prerequisites` references an earlier one, substitute the real `gh-<number>` id into its spec block before creating the dependent. Report the created issue URLs. The operator runs them with `flywheel worker` and watches with `flywheel status` / `fw`; outcomes are posted back to each issue as a comment.
