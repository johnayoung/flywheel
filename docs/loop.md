# Loop

The loop is the control plane around an AI coding agent. The agent is the brain; the loop owns the execution lifecycle of a single [task](task-schema.md).

It does not queue, plan, or decompose work. It runs one task: invoke the agent, interpret its signals, verify completion claims, record history, and expose intervention points.

## Responsibilities

- Invoke the agent and carry context across iterations
- Own [lifecycle](task-lifecycle.md) transitions (the agent never mutates state directly)
- Detect iteration state from SDK signals and the intent envelope
- Verify completion claims before reaching `done`
- Preserve attempt history and failure context
- Emit structured events for telemetry

## State machine

```
pending -> ready -> running -> validating -> done
                      |           |
                      |      failed_validation -> ready (retry) or failed
                      |
                      +-> failed
                      +-> interrupted -> ready
```

`completed` from the agent starts `validating`, not `done`. Verification outcome and retry policy are separate decisions.

## State detection

Detection combines SDK-native signals (`StopReason`, hooks, typed messages) with a minimal intent envelope from the agent. Judgment-heavy logic — the verifier, thrash analysis, crash classification — lives in pluggable components the loop calls into, not in the core state machine.

| State | Description | SDK surface | Loop service |
|---|---|---|---|
| `done` | Agent's turn ended cleanly and it believes the work is complete. Candidate for verification, not final. | `AssistantMessage.StopReason == "end_turn"` with no pending `ToolUseBlock`; `ResultMessage` with `IsError: false` | Run the task's graders in cost order and promote to lifecycle `done` only on all-pass. **TODO — grader implementations:** the `command` runner is trivial, but the `rubric` LLM verifier and `manual` approval surface are their own subsystems requiring separate research phases. |
| `working` | Agent is making productive progress — tool calls succeeding, novel actions, diff growing. Default healthy state. | `PostToolUse` hook fires; `ToolResultBlock.IsError == false`; `TaskProgressMessage` arriving | **Simple:** reset watchdog timer, append event to progress window, continue. Pure bookkeeping. |
| `blocked_explicit` | Agent has declared it needs external input to proceed. | Text content in `AssistantMessage`; `NotificationArrived` hook; simplified intent envelope | **Simple** if the intent envelope is a closed enum (`blocked`, `verify`, `continue`, `abort`) — trivial parse and pause. **Open question:** how the `reason` field is handled without it becoming free-form conditional logic inside the loop. |
| `blocked_implicit` | Agent is stuck but doesn't know it — same tool failing repeatedly, same permission denied, same question re-asked. | `PostToolUseFailure` hook; `ToolResultBlock.IsError == true`; `ResultMessage.PermissionDenials` | **Simple for the mechanical case:** counter keyed on `(tool_name, sha256(input))`, tripped after repeated consecutive failures. Same shape for permission denials on `(tool_name, resource)`. **TODO — "same question re-asked":** requires semantic similarity between assistant text blocks; not deterministic. Research options include embedding-model similarity with a threshold study; each requires its own research phase. |
| `thrashing` | Tool calls succeed but produce no net progress — same files edited with zero-sum diffs, input novelty collapsing. | `PostToolUse` hook with tool name + input; `WithFileCheckpointing` for diff observation | **TODO — genuinely complex.** Three sub-problems with different difficulty: (a) tuple repetition across a rolling window — deterministic; (b) net-diff-near-zero — needs filesystem snapshots and a "meaningful change" heuristic (whitespace? comments? line count threshold?); (c) input novelty score — needs a distance metric over structured inputs, which is research. No ordering, defaults, or thresholds prescribed; each sub-problem requires its own research phase. |
| `hanging` | No output of any kind for longer than threshold — not thinking, not rate-limited, just silent. | Streaming iterator delivers messages one-by-one; `ThinkingBlock` and `RateLimitEvent` are valid liveness signals | **Simple mechanism:** a watchdog timer reset on every message type, fired if silent past threshold. **Open question:** threshold value — requires research informed by extended-thinking budgets and telemetry before a default can be set. |
| `rate_limited` | Agent is waiting on API quota. Looks like hanging but is transient and expected. | `RateLimitEvent` with `RateLimitInfo` | **Simple:** flip a `rate_limited` flag that the watchdog checks before firing; compute ETA from `RateLimitInfo.ResetsAt` and emit to operator surface. Clear flag on next non-rate-limit message. |
| `context_exhausted` | Conversation outgrew the context window. | `StopReason == "max_tokens"` on assistant or result; `Client.GetContextUsage()` approaching cap | **Simple — detection.** **TODO — recovery policy:** compacting (via `PreCompact` hook), summarize-and-restart, and fork-with-smaller-context are genuinely different strategies with different correctness tradeoffs. No default or sequencing — each candidate strategy requires its own research phase. |
| `budget_exceeded` | Hit configured turn or cost ceiling. | `WithMaxTurns` / `WithMaxBudgetUSD` enforce; `ResultMessage.NumTurns` and `TotalCostUSD` for observation | **Simple:** SDK halts the agent automatically. Loop records cause, applies retry policy from task config (`max_retries`, `backoff`). Straightforward config-driven logic. |
| `crashed` | Subprocess died, SDK errored, or transport failed. | Iterator returns `err`; global `Error` hook; context cancellation | **Simple — recording and surfacing.** **TODO — classification between infra / agent / protocol.** Error types from the SDK don't cleanly map to those categories; classification likely needs to inspect error wrapping, exit codes, and whether a partial envelope was received. Candidate heuristics (context deadline → infra; non-zero exit without iterator error → agent; malformed envelope → protocol) are research points, not defaults; a corpus of real crashes is needed before any classifier can be designed. |

## Iteration envelope

The envelope carries agent intent only; observed state comes from SDK signals per the detection map above. Every iteration ends with a JSON envelope delimited by `<!-- LOOP_STATUS -->`.

```html
<!-- LOOP_STATUS -->
{
  "intent": "verify" | "blocked" | "continue" | "abort",
  "reason": "..."
}
<!-- /LOOP_STATUS -->
```

`intent=blocked` MUST additionally carry a non-empty `requires` array describing what would unblock the lifecycle. Non-blocked intents omit `requires`; any payload-level `requires` on `verify`, `continue`, or `abort` is ignored. Three predicate shapes are recognized in v1 (and only these three):

```json
{"type": "command_grader", "name": "<grader-name>"}
{"type": "file_exists", "path": "<path>", "present": true}
{"type": "env_var_set", "name": "<ENV_VAR>"}
```

`file_exists.present` defaults to `true` when omitted. A blocked envelope without `requires`, with a non-list `requires`, with an empty list, or with an entry of unknown `type` or missing per-type field is a protocol failure.

The envelope is untrusted protocol input. The harness must handle malformed JSON, missing fences, duplicates, truncation, and contradictory claims as first-class cases.

## Rubric verdict envelope

Rubric graders run an LLM-as-judge in a fresh `claude-agent-sdk` session and require the judge to terminate its response with one fenced JSON block:

```html
<!-- RUBRIC_VERDICT -->
{"passed": true, "summary": "<one or two sentences>", "unknown": false}
<!-- /RUBRIC_VERDICT -->
```

- `passed` (bool, required) — true if every assertion holds.
- `summary` (str, required) — brief rationale; empty string accepted.
- `unknown` (bool, optional, default false) — set true only when evidence is insufficient. Counts as a pass for lifecycle purposes; surfaced via `harness.rubric_unknown`.

The verdict is untrusted protocol input. The parser returns a closed taxonomy and each non-valid variant routes through `INTERNAL_ERROR` (judge-infrastructure failure, retry-eligible):

- `MissingVerdict` — no opening fence in the judge's response.
- `TruncatedVerdict` — opening fence with no matching closing fence.
- `DuplicateVerdict` — more than one fence pair (guards against prompt-injection style fake verdicts).
- `MalformedVerdict` — JSON decode failure or field-level shape violation (missing/wrong-type `passed`, `summary`, or `unknown`).

When a rubric `passed=false` verdict triggers an auto-retry (`retry_on_fail=True`), its `summary` is carried into the next attempt's prompt as a `# Reviewer feedback` section so the working agent sees the critique on its next iteration.

## Harness behavior

Per-state dispatch is specified by the **Loop service** column of the detection map above. Each detected state maps to a concrete loop action — bookkeeping, watchdog reset, verification orchestration, pause-for-intervention, retry policy, or fail-loud. Complex cases (thrash detection, crash classification, context-recovery policy) are flagged as TODO subsystems in that column.

Every harness `EventRecord` and every SDK `Message` observed during an iteration is persisted under a single per-run monotonic sequence; `flywheel.audit` is the canonical replay and live-inspection surface for that stream.

## Graders

Triggered when the agent claims `completed`. Driven by the task's `graders` list. Run in cost order; first failure inside a type skips the rest of that type and later types.

1. **`command`** — deterministic shell check (tests, build, lint, typecheck, state assertions). MVP: failures retry automatically.
2. **`transcript`** — path-level constraints (`max_turns`, `max_total_tokens`, `max_wall_seconds`). Also enforced as hard limits during the run, not only at grade time. MVP: failures retry automatically.
3. **`rubric`** — separate LLM verifier evaluates natural-language assertions against the goal, diff, and artifacts. MVP: failures pause for operator review.
4. **`manual`** — surfaces summary + artifacts for human approval. MVP: rejections pause for operator decision.

A failed grader records `failed_validation`. What happens next is policy.

## Intervention

Agent-reported:

- **Blocked** — agent-reported need for external input. Resumes via `ready`.
- **Interrupted** — external pause/stop. Resumes via `ready`.

Loop-initiated (detected, not self-reported by the agent):

- **blocked_implicit** — loop detected repeated tool failure or permission denials; agent did not self-report.
- **thrashing** — loop detected no-net-progress pattern (same `(tool, target)` tuple repeats, zero-sum diffs).
- **hanging** — watchdog timer expired with no message traffic and no valid liveness signal.

## Failure classes

Recorded distinctly even when lifecycle states overlap:

| Class              | Cause                                                               |
| ------------------ | ------------------------------------------------------------------- |
| Task failure       | Agent could not complete                                            |
| Validation failure | Completion claim disproved by any grader (command, transcript, rubric, manual) |
| Protocol failure   | Malformed, missing, duplicate, or truncated envelope                |
| Infrastructure     | SDK, subprocess, verifier, or storage failure                       |

Each detected state maps to a primary class. Notes capture classification shifts that can only be settled post-mortem:

| State                       | Primary class      | Notes                                                                 |
| --------------------------- | ------------------ | --------------------------------------------------------------------- |
| `blocked_explicit`          | Task failure       | —                                                                     |
| `blocked_implicit`          | Task failure       | —                                                                     |
| `thrashing`                 | Task failure       | —                                                                     |
| `hanging`                   | Infrastructure     | Indistinguishable from subprocess failure at detection time           |
| `context_exhausted`         | Task failure       | Reclassify to Infrastructure if recovery policy fails repeatedly      |
| `budget_exceeded`           | Task failure       | —                                                                     |
| `crashed`                   | Infrastructure     | Refine to Protocol on malformed envelope, Task on clean non-zero exit |
| Malformed envelope          | Protocol failure   | —                                                                     |
| Grader rejection            | Validation failure | —                                                                     |

`rate_limited` is transient and not a failure class. `working` and pre-verification `done` are not failures either.

## Non-goals

- Not a queue — does not schedule or prioritize across tasks
- Not a planner — does not decompose goals into subtasks
- Not a UI — operational surfaces are minimal; richer views consume the event stream

## Surface

A Python library, embeddable in CI pipelines, task queues, and orchestrators.

## Requirements

- [`claude-agent-sdk`](https://github.com/anthropics/claude-agent-sdk-python) — the execution substrate for driving Claude Code as a subprocess. The loop invokes the agent through this SDK.
