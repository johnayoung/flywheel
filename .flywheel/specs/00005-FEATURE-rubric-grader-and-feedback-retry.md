# Feature: Rubric Grader Execution and Retry-with-Feedback

## Summary

Wire the dormant `rubric` grader type into the harness as an LLM-as-judge runner that performs a fresh, isolated `claude-agent-sdk` call against the attempt's worktree and emits a structured pass/fail verdict. Replace the deferred docs/vision.md:132 "pause for operator review" default with auto-retry-with-feedback: rubric findings persist via the existing `GraderResultRecord` pipeline, then surface in the next attempt's prompt so the working agent gets the judge's critique on its next iteration.

## Background

Today `RubricGrader` (`src/flywheel/task.py:22`) and `ManualGrader` are first-class dataclasses validated by `flywheel.task` and parsed by `flywheel.loaders` (`src/flywheel/loaders.py:98-101`), but the harness only executes `CommandGrader` and `TranscriptGrader` (`src/flywheel/harness.py:892-924`). A task declaring a rubric ships with a grader that never runs — a documented contract the user can configure but the harness silently ignores.

The feature lifts rubric out of the dead-letter set by adding a new runner (`grader_rubric.py`), a verdict envelope contract mirroring the LOOP_STATUS discipline already used for working-agent envelopes, and a retry policy that closes the loop: when the judge fails the work, the harness records the findings, starts a new attempt under the existing retry budget, and the prompt builder pulls the most recent rubric findings into the next iteration's prompt.

Motivated by parity with an external CLI-driven worker that runs `/review --post-task` after each task and feeds findings back as task input on failure — that pattern measurably reduces false-positive "passing" claims. Flywheel's rubric grader is the right architectural seat for the same idea, and the rubric judge already has access to the working agent's worktree via the per-task workspace isolation introduced in commit `8270300`.

The CLAUDE.md invariant ("Task is immutable; execution-time clarifications live in lifecycle records") rules out mutating the `Task` to carry feedback. The grader-results store is already the system of record for grader receipts, including the documented `rubric` payload shape in `_schema/persistence-schema.sql:73`, so it becomes the single source of truth for feedback that the prompt builder reads from on retry.

## Scope

### In Scope

- New runner module `src/flywheel/grader_rubric.py` exposing `run_rubric_graders(task, store, ..., transcript, worktree, judge_invoke=None, now=None) -> list[GraderResultRecord]`.
- A separate `claude-agent-sdk` call per `RubricGrader` instance: fresh session (no `session_id` inheritance), cwd set to the attempt's worktree, full tool surface (same as the working agent), bounded by a turn cap.
- New fenced verdict envelope contract `<!-- RUBRIC_VERDICT -->` carrying `{passed: bool, summary: str, unknown?: bool}`; parser lives in `grader_rubric.py` and reuses the `Missing/Malformed/Truncated` taxonomy pattern from `flywheel.envelope`.
- Two new optional fields on `RubricGrader`: `judge_model: str | None = None` and `retry_on_fail: bool = True`. Loader passes them through; defaults preserve schema-back-compat.
- Two new fields on `HarnessConfig`: `rubric_judge_model: str | None = None` (default judge model when grader omits one) and `rubric_judge_max_turns: int = 8` (per-judge-call cap).
- Harness wiring in `_validate` (`src/flywheel/harness.py:892`): after command + transcript graders pass, run rubric graders in cost order (already documented at docs/vision.md:142-149); skip remaining rubrics on first failure within the type.
- Retry-with-feedback path: on `retry_on_fail=True` rubric failure, the existing `VALIDATING -> FAILED_VALIDATION -> READY -> RUNNING` cycle fires; `IterationInputs` carries the prior attempt's rubric findings; `build_iteration_prompt` surfaces them in a new `# Reviewer feedback` section.
- Unknown verdict path: `unknown=true` counts as a pass for lifecycle purposes; harness emits `harness.rubric_unknown` event so operators can audit punt rates.
- Judge-infra failure path: SDK crash, rate limit, malformed verdict envelope routes through `INTERNAL_ERROR` (retry-eligible, same budget as agent crashes).
- New harness events: `harness.rubric_invoked` (per-rubric-call, payload `{grader_name, judge_model, attempt_number}`), `harness.rubric_verdict` (post-parse, payload mirrors the verdict envelope), `harness.rubric_unknown` (only when `unknown=true`).
- Update docs: `docs/vision.md:132` (retract the "pauses for operator review" MVP statement), `docs/vision.md:149` (add rubric to the auto-retry list under the `retry_on_fail=True` default), `docs/loop.md` (document the verdict envelope), `docs/task-schema.md` (document new RubricGrader fields).

### Out of Scope

- Implementing `ManualGrader` execution. Manual graders remain a `TODO` after this feature lands; `docs/vision.md:136-138` continues to apply to them.
- Changing `CommandGrader` or `TranscriptGrader` behavior; both runners are untouched.
- Modifying `.workflow/task-worker.sh` (already updated in a prior session).
- A per-task `retry_on_fail` switch on the `Task` itself — per-grader is the chosen seat.
- A separate `max_rubric_retries` budget — rubric retries share `max_retries` with command/transcript per the existing `_RETRY_SOURCE_STATUSES` model.
- Persisting the judge's session id, transcript, or raw SDK signals (only the parsed verdict + structured payload).
- A `Read`/`Grep`-only judge tool variant. The judge gets the same tool surface as the working agent in v1; future hardening may restrict if mutation pollutes downstream graders.
- Multiple judgments by the same `RubricGrader` instance within one Attempt. Each `RubricGrader` runs exactly once per Attempt; multiple rubric graders on the same task each run once.
- A schema migration to `grader_results` — the table's `payload_json` shape for `rubric` is already documented at `_schema/persistence-schema.sql:73` and is reused as-is.
- Operator dashboard UI for rubric findings (consumer concern; surfaced via existing `flywheel status` and event log).

## Requirements

### Functional Requirements

1. **FR-1 — Rubric runner module.** A new module `src/flywheel/grader_rubric.py` exports `run_rubric_graders(task, store, *, run_id, attempt_number, transcript, worktree, command_passed, transcript_passed, judge_invoke=None, judge_model=None, judge_max_turns=8, now=None) -> list[GraderResultRecord]`. The runner:
   - Iterates `task.graders` in list order; skips non-`RubricGrader` entries; preserves the ordinal of the grader in `task.graders`.
   - When `command_passed` or `transcript_passed` is False, the runner returns `[]` without invoking any judge (cost-order short-circuit; matches the existing `_validate` discipline).
   - For each `RubricGrader`: invokes the judge via `judge_invoke` (production default delegates to `claude_agent_sdk.query`), captures the response, parses the verdict envelope, builds a `GraderResultRecord` whose `payload` matches the schema's rubric shape, and appends to `store`.
   - Aborts the remaining rubric graders on the first failed or unknown-as-fail verdict (cost-order within type).
   - **Acceptance:** new file `tests/test_grader_rubric.py` covers: (a) all-pass path persists one record per `RubricGrader`; (b) first-failure short-circuit skips subsequent rubric graders; (c) command-failure short-circuit returns `[]`; (d) non-rubric graders are ignored without persisting rows; (e) ordinal matches the grader's index in `task.graders`.

2. **FR-2 — Verdict envelope contract.** The judge is prompted to terminate its response with exactly one fenced JSON block of the form:
   ```html
   <!-- RUBRIC_VERDICT -->
   {"passed": true|false, "summary": "...", "unknown": false}
   <!-- /RUBRIC_VERDICT -->
   ```
   `unknown` defaults to `false` when omitted. The parser in `grader_rubric.py` returns a tagged union (`ValidVerdict`, `MissingVerdict`, `MalformedVerdict`, `DuplicateVerdict`, `TruncatedVerdict`) mirroring `flywheel.envelope`. The module is pure — no IO, no SDK imports.
   - **Acceptance:** `tests/test_grader_rubric.py` exercises each parse case: well-formed (pass / fail / unknown), missing fences, malformed JSON, missing `passed` field, non-bool `passed`, duplicate fences, truncated trailing fence.

3. **FR-3 — Judge SDK call.** The production `judge_invoke` callable issues a fresh `claude_agent_sdk.query`:
   - `cwd = worktree` (the attempt's per-task git worktree).
   - `model = grader.judge_model or HarnessConfig.rubric_judge_model` (when both are `None`, the SDK's own default applies).
   - `max_turns = HarnessConfig.rubric_judge_max_turns` (default 8).
   - `permission_mode = "bypassPermissions"`, `skills = "all"`, same tool surface as the working agent (full tool set per the answered design).
   - No `session_id` set — judge starts cold; never inherits the working agent's session.
   - Prompt contains: rendered task goal, the rubric assertions, the working agent's transcript (verbatim from `iteration_result.transcript`), and the verdict envelope contract.
   - **Acceptance:** `tests/test_grader_rubric.py` injects a fake `judge_invoke` and asserts: (a) judge receives goal + assertions + transcript in its prompt; (b) judge call uses fresh session (no `session_id` in the synthetic options); (c) judge cwd matches the supplied worktree; (d) model precedence is `grader.judge_model > HarnessConfig.rubric_judge_model > None`.

4. **FR-4 — Harness integration.** `_validate` in `src/flywheel/harness.py` runs rubric graders after command + transcript and before transitioning to `DONE`. The integration:
   - Resolves the attempt's worktree (already available via the workspace-isolation work in commit `8270300`; plumbed through the harness's existing artifact / cwd plumbing).
   - Invokes `run_rubric_graders` with `command_passed`, `transcript_passed`, the iteration transcript, and the worktree path.
   - When all rubric verdicts are `passed=true` (or `unknown=true` per FR-6): transitions `VALIDATING -> DONE`.
   - When any rubric verdict is `passed=false`: routes via `_grader_failure_error` (which already reads `result.grader_type` / `result.grader_name`) and transitions `VALIDATING -> FAILED_VALIDATION`. The retry path is the existing `is_retry_eligible` check.
   - When a rubric's grader has `retry_on_fail=False`: transitions `VALIDATING -> INTERRUPTED` instead (preserves the original docs/vision.md:132 operator-review behavior as an opt-in). The lifecycle's `error` records the rubric name; the `GraderResultRecord` carries the findings.
   - **Acceptance:** `tests/test_harness.py` covers: (a) all-pass rubric path reaches `DONE`; (b) rubric-fail with `retry_on_fail=True` reaches `FAILED_VALIDATION` and consumes one retry; (c) rubric-fail with `retry_on_fail=False` reaches `INTERRUPTED` without consuming a retry; (d) rubric-fail with retries exhausted reaches `FAILED` with `error` containing the rubric name + findings summary; (e) command-fail short-circuits before rubric runs (no `harness.rubric_invoked` event); (f) `harness.rubric_invoked` and `harness.rubric_verdict` events are emitted with the correct attempt_number.

5. **FR-5 — Retry-with-feedback prompt.** `IterationInputs` (`src/flywheel/prompt.py:42`) gains a new field `prior_rubric_findings: tuple[RubricFindings, ...] = ()` where `RubricFindings` is a small frozen dataclass `{grader_name: str, attempt_number: int, summary: str}`. `build_iteration_prompt` renders a new `# Reviewer feedback` section before the `# Verification` section when the tuple is non-empty. The harness populates the tuple by querying `store.list_grader_results` for the most recent finalized attempt and selecting `grader_type == "rubric" and not passed`.
   - **Acceptance:** `tests/test_prompt.py` covers: (a) empty tuple renders no `# Reviewer feedback` section (byte-identical to today's output); (b) one finding renders one block with grader name + attempt number + summary; (c) multiple findings render in attempt-then-ordinal order; (d) deterministic output (byte-identical for byte-identical inputs). `tests/test_harness.py` covers the end-to-end: a failing first attempt with rubric findings, followed by a second attempt whose prompt contains the findings.

6. **FR-6 — Unknown verdict path.** When the parsed verdict has `unknown=true`, the runner treats it as a pass (rubric does not block the lifecycle), persists the `GraderResultRecord` with `passed=true` and `payload.unknown=true`, and emits `harness.rubric_unknown` with `{grader_name, summary}`. The lifecycle continues to `DONE` (assuming other graders pass).
   - **Acceptance:** `tests/test_grader_rubric.py` covers the unknown verdict; `tests/test_harness.py` covers the end-to-end where `unknown=true` reaches `DONE` and emits the warning event.

7. **FR-7 — Judge-infrastructure failure path.** When the judge SDK call raises, rate-limits, returns no message, or returns a verdict that fails to parse (`MissingVerdict`, `MalformedVerdict`, `DuplicateVerdict`, `TruncatedVerdict`), the runner raises a typed `RubricJudgeError`. The harness catches it inside `_validate`, transitions `VALIDATING -> INTERNAL_ERROR` with an error of the form `"rubric judge failed: <grader_name>: <reason>"`, and the existing retry-budget logic applies (matches the agent-crash path at `harness.py:622-637`).
   - **Acceptance:** `tests/test_grader_rubric.py` covers each parse failure raising `RubricJudgeError`. `tests/test_harness.py` covers: (a) judge-crash routes through `INTERNAL_ERROR`; (b) retry-eligible after one judge crash; (c) retries exhausted on repeated judge crashes reaches `FAILED`.

8. **FR-8 — Schema fields.** `RubricGrader` gains two optional kwargs:
   - `judge_model: str | None = None`
   - `retry_on_fail: bool = True`
   `flywheel.loaders.parse_grader` recognizes both (unknown fields continue to be rejected per the existing strict schema). `HarnessConfig` gains `rubric_judge_model: str | None = None` and `rubric_judge_max_turns: int = 8`.
   - **Acceptance:** `tests/test_task.py` covers the new fields' defaults and round-trip. `tests/test_loaders.py` covers parsing both fields and rejecting unknowns. `tests/test_harness.py` covers the precedence rule in FR-3(d) end-to-end.

### Non-Functional Requirements

- **Purity invariants preserved.** `flywheel.task`, `flywheel.lifecycle`, and `flywheel.prompt` remain pure (no `json`/`pathlib`/`io`/SDK imports). The new `grader_rubric.py` is *not* pure — it imports the SDK and shells out to git via the judge — but it lives alongside `grader_command.py` which is also IO-bearing. The verdict parser inside `grader_rubric.py` is pure relative to the SDK call (pure-function `parse_verdict(text: str) -> VerdictResult`). The `RubricFindings` dataclass on `IterationInputs` is pure data.
- **Performance.** A rubric judge call is bounded by `rubric_judge_max_turns` (default 8) and the SDK's own context-window ceiling. Typical no-tool verdicts complete in 1 turn; the cap exists to allow tool-using judgments without unbounded runaway. Cost order is preserved (command -> transcript -> rubric -> manual), so deterministic failures still abort before any LLM cost.
- **Concurrency.** No new concurrency surface. Optimistic-concurrency on `Lifecycle.version` continues to gate the `VALIDATING -> *` transitions. The rubric runner is synchronous within an attempt; multiple rubrics on the same task run sequentially.
- **Security.** The judge runs in the attempt's worktree with `permission_mode="bypassPermissions"` and full tool surface — same blast radius as the working agent. Worktree isolation contains mutation. Judges are prompted to verdict only; if they mutate the worktree, downstream graders within the same attempt are not re-run (the rubric is the last grader type before `DONE`). The verdict envelope is parsed with the same Missing/Malformed/Truncated discipline as LOOP_STATUS, so prompt-injection attempts that emit fake verdicts are caught when they collide with the real verdict (DuplicateVerdict).
- **Telemetry.** Every rubric invocation is observable: `harness.rubric_invoked` (start), `harness.rubric_verdict` (end), `harness.rubric_unknown` (only when applicable). The `GraderResultRecord` continues to be the durable receipt.
- **Cost order documentation.** `docs/vision.md:132` (the "pauses for operator review" statement) becomes obsolete and must be retracted in the same change. `docs/vision.md:149` (the auto-retry list) must add `rubric` under the `retry_on_fail=True` default.

## Behavior Specification

### Happy Path

1. A task declares a `RubricGrader`:
   ```python
   RubricGrader(
       name="implementation-matches-goal",
       assertions=[
           "The change implements the goal as stated.",
           "No unrelated files were modified.",
       ],
       # judge_model and retry_on_fail use defaults.
   )
   ```
2. The working agent emits `intent=verify`; command + transcript graders pass; the harness invokes `run_rubric_graders`.
3. The runner issues a fresh `claude_agent_sdk.query` with cwd = the attempt's worktree, prompt containing the goal + assertions + the working agent's transcript + the verdict-envelope contract.
4. The judge runs `git diff HEAD` via its Bash tool, reads a few files via Read, and emits:
   ```html
   <!-- RUBRIC_VERDICT -->
   {"passed": true, "summary": "Diff matches the goal; only the two declared files changed."}
   <!-- /RUBRIC_VERDICT -->
   ```
5. The runner parses the verdict, persists a `GraderResultRecord(grader_type="rubric", passed=True, payload={...})`, and emits `harness.rubric_invoked` + `harness.rubric_verdict`.
6. All rubric verdicts are `passed=true`; the harness transitions `VALIDATING -> DONE`.

### Retry-with-Feedback Path

1. Same setup as above, but the judge returns:
   ```html
   <!-- RUBRIC_VERDICT -->
   {"passed": false, "summary": "The change modifies src/foo.py but the goal asked for src/bar.py. The implementation does not address the stated requirement."}
   <!-- /RUBRIC_VERDICT -->
   ```
2. The runner persists `GraderResultRecord(grader_type="rubric", passed=False, payload={..., "summary": "..."})`.
3. The harness transitions `VALIDATING -> FAILED_VALIDATION` with `error = "rubric grader 'implementation-matches-goal' failed"`.
4. `is_retry_eligible(max_retries)` returns `True` (one retry remaining).
5. The harness transitions `FAILED_VALIDATION -> READY`, emits `harness.retry_scheduled`, and starts a new Attempt.
6. Before invoking the working agent, the harness queries `store.list_grader_results` for the prior attempt, filters to failing rubrics, and assembles `IterationInputs.prior_rubric_findings`.
7. `build_iteration_prompt` renders a new section:
   ```markdown
   # Reviewer feedback

   The reviewer flagged the following on attempt #1:

   - rubric `implementation-matches-goal`: The change modifies src/foo.py but the goal asked for src/bar.py. The implementation does not address the stated requirement.
   ```
8. The working agent reads the feedback, corrects course, emits `intent=verify`; this iteration's diff modifies `src/bar.py`; rubric passes; lifecycle reaches `DONE`.

### Error Handling

| Error Condition                                                       | Expected Behavior                                                                                                                                                                                                          |
| --------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Judge SDK raises (ProcessError, network, rate-limit)                  | Runner raises `RubricJudgeError`. Harness transitions `VALIDATING -> INTERNAL_ERROR`; retry-eligible per existing budget. `harness.crash` event emitted with `classification: "rubric_judge_error"`.                       |
| Judge returns no message / empty transcript                           | `MissingVerdict` parse result. Same path as SDK raise: `INTERNAL_ERROR`, retry-eligible.                                                                                                                                   |
| Judge verdict is malformed JSON or missing `passed`                   | `MalformedVerdict`. Same path: `INTERNAL_ERROR`.                                                                                                                                                                           |
| Judge emits two verdict envelopes in one response                     | `DuplicateVerdict`. Same path: `INTERNAL_ERROR`. Prevents prompt-injection style fake verdicts from masking a real one.                                                                                                    |
| Judge emits opening fence but no closing fence                        | `TruncatedVerdict`. Same path: `INTERNAL_ERROR`.                                                                                                                                                                           |
| Judge returns `passed=false` with `retry_on_fail=True`                | `VALIDATING -> FAILED_VALIDATION`. Retry-with-feedback path applies. Findings surface in the next attempt's prompt.                                                                                                        |
| Judge returns `passed=false` with `retry_on_fail=False`               | `VALIDATING -> INTERRUPTED`. Lifecycle parks for operator review. Findings live in `GraderResultRecord.payload`. Preserves the original docs/vision.md:132 behavior as an opt-in.                                          |
| Judge returns `unknown=true`                                          | Treated as pass; `harness.rubric_unknown` event emitted. Lifecycle proceeds to `DONE` (assuming other rubrics also pass).                                                                                                  |
| Retry budget exhausted on a rubric failure                            | `FAILED_VALIDATION -> FAILED`. `error` contains the rubric grader name and the latest summary text. Findings remain queryable via `store.list_grader_results`.                                                             |
| Command or transcript grader fails before rubric runs                 | Rubric is not invoked (cost-order short-circuit). No `harness.rubric_invoked` event emitted. Lifecycle takes the existing command/transcript failure path.                                                                 |
| Worktree path is missing (workspace-isolation failure upstream)       | Runner raises `RubricJudgeError("worktree not available")`. Harness routes via `INTERNAL_ERROR`. (Practically should not happen since the isolation feature creates worktrees before `_validate`; defense in depth.)        |
| Judge mutates the worktree mid-judgment                               | Allowed in v1 (full tool surface). Mutation does not re-run prior graders within the same attempt (rubric is last in cost order). Will be surfaced in the next attempt's `git diff` if the lifecycle retries. Documented risk. |

### Edge Cases

| Case                                                                                                  | Expected Behavior                                                                                                                                                                                                                                  |
| ----------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Task has multiple `RubricGrader` instances                                                            | Each runs once per Attempt, in `task.graders` list order. First failure short-circuits the rest within the rubric type (matches existing within-type cost-order rule). Each receipt is a separate `GraderResultRecord` keyed by ordinal.            |
| `RubricGrader.judge_model = "claude-haiku-4-5"` overrides `HarnessConfig.rubric_judge_model = "claude-opus-4-7"` | Per-grader model wins. Persisted in the `GraderResultRecord.payload.judge_model` field.                                                                                                                                                            |
| All-default config: no per-grader and no harness-level model                                          | `model=None` is passed to the SDK; SDK's own default applies. `payload.judge_model` records what the SDK reported (via the result message), not the request value.                                                                                  |
| `prior_rubric_findings` for the first attempt of a lifecycle                                          | Empty tuple. `# Reviewer feedback` section is not rendered. Prompt output is byte-identical to today's first-attempt prompt.                                                                                                                       |
| Second attempt where prior attempt's rubric never ran (command grader failed)                         | `prior_rubric_findings` is empty (no failing rubric record exists for prior attempt). Working agent sees the command grader's error via the existing lifecycle.error path; no reviewer-feedback section.                                            |
| Lifecycle resumed from INTERRUPTED (rubric retry_on_fail=False)                                       | Operator clears the interrupt manually (existing path) or via the recheck primitive once spec 00004 lands. Resumption starts a fresh Attempt; `prior_rubric_findings` populates from the last failing rubric record so the agent sees the critique. |
| Judge runs out of turns (hits `rubric_judge_max_turns`) without emitting a verdict                    | SDK returns a final message; verdict parser yields `MissingVerdict`. Routes via `INTERNAL_ERROR` (judge could not verdict). Retry-eligible.                                                                                                        |
| Verdict's `summary` is empty string                                                                   | Accepted at parse time. The reviewer-feedback section renders the grader name + "(no summary provided)" placeholder so the agent still knows the rubric failed.                                                                                    |
| `RubricGrader.retry_on_fail=False` mixed with `retry_on_fail=True` rubric on the same task            | First failure (in `task.graders` order) determines the lifecycle's exit transition. `retry_on_fail=False` failure trumps a later True (because rubrics short-circuit on first fail). Tests pin this ordering.                                       |
| Loader receives `retry_on_fail` as a string ("true" / "false")                                        | Loader rejects: `retry_on_fail` must be a bool per the strict-parsing pattern in `flywheel.loaders`. Matches existing field-type strictness.                                                                                                       |

## Technical Context

### Affected Modules

- `src/flywheel/grader_rubric.py` — **NEW**. The runner, the verdict parser, the `RubricJudgeError` exception, the `RubricFindings` dataclass (or moved to `flywheel.prompt` per the prompt-purity invariant — TBD during implementation). Mirrors the surface of `grader_command.py`.
- `src/flywheel/task.py` — extend `RubricGrader` with `judge_model: str | None = None` and `retry_on_fail: bool = True`. Pure module; no other change.
- `src/flywheel/loaders.py` — pass through the two new fields in `parse_grader`'s rubric branch. Reject unknown fields per existing strict-parsing.
- `src/flywheel/harness.py` — wire `run_rubric_graders` into `_validate` after the existing command + transcript pass. Add `rubric_judge_model` and `rubric_judge_max_turns` to `HarnessConfig`. Pass the worktree path into the runner. Catch `RubricJudgeError` and route via `INTERNAL_ERROR`. Populate `IterationInputs.prior_rubric_findings` in `_drive_iterations` before each prompt build.
- `src/flywheel/prompt.py` — extend `IterationInputs` with `prior_rubric_findings: tuple[RubricFindings, ...] = ()`. Add `_section_reviewer_feedback(findings)`. Render the new section between `# Context` and `# Verification`. Must stay pure — `RubricFindings` is pure data only.
- `src/flywheel/_schema/persistence-schema.sql` — **NO CHANGE**. The `rubric` payload shape is already documented at line 73; the runner emits a payload matching it. Postgres mirror also unchanged.
- `docs/vision.md` — retract the line 132 "pauses for operator review" MVP statement; update the line 149 auto-retry list to include `rubric` under the `retry_on_fail=True` default; note the per-grader opt-out.
- `docs/loop.md` — add a new section "Rubric verdict envelope" documenting the `<!-- RUBRIC_VERDICT -->` fenced JSON shape, the `{passed, summary, unknown?}` fields, and the protocol-failure taxonomy.
- `docs/task-schema.md` — document the new `RubricGrader` fields (`judge_model`, `retry_on_fail`) and their defaults.

### Integration Points

- **Per-task workspace isolation** (commit `8270300`): the rubric runner depends on the attempt's worktree existing. The runner receives the path via a kwarg from the harness; it does not re-derive it. If the upstream isolation fails to provide a worktree, the runner raises `RubricJudgeError` (defense in depth).
- **Existing `GraderResultRecord` pipeline**: rubric receipts use the existing `append_grader_result` verb; no new store method. The payload shape is the one already documented at `_schema/persistence-schema.sql:73`.
- **Existing retry budget**: `Lifecycle.is_retry_eligible(max_retries)` and `_RETRY_SOURCE_STATUSES` are unchanged. Rubric failures with `retry_on_fail=True` route through `FAILED_VALIDATION`, which is already a retry source. Rubric failures with `retry_on_fail=False` route through `INTERRUPTED`, which is recoverable but does not consume the retry budget — matches the existing `intent=blocked` behavior.
- **`claude_agent_sdk.query`**: judge calls reuse the same SDK as the working agent. Different `ClaudeAgentOptions` (cwd = worktree, max_turns = rubric_judge_max_turns, model = judge model). No session inheritance.
- **Prompt determinism**: `build_iteration_prompt` must remain byte-deterministic. `RubricFindings` is a frozen dataclass; iteration order in the rendered section is `(attempt_number, ordinal)` ascending.

### Relevant Existing Code

- `src/flywheel/harness.py:892-996` — `_validate`: the integration seat for the new rubric runner.
- `src/flywheel/harness.py:144-180` — `HarnessConfig`: gains `rubric_judge_model` and `rubric_judge_max_turns`.
- `src/flywheel/harness.py:485-515` — the retry-eligibility loop; unchanged but reused.
- `src/flywheel/harness.py:622-637` — the agent-crash `INTERNAL_ERROR` path; same path used for `RubricJudgeError`.
- `src/flywheel/grader_command.py` — structural template for `grader_rubric.py`'s runner signature and `GraderResultRecord` assembly.
- `src/flywheel/envelope.py` — discipline template for the verdict envelope parser (tagged union of `Valid/Missing/Malformed/Duplicate/Truncated`).
- `src/flywheel/prompt.py:42-77` — `IterationInputs` and `build_iteration_prompt`: the seat for the new findings field and section.
- `src/flywheel/prompt.py:155-193` — `_section_lifecycle`: structural template for the new `_section_reviewer_feedback`.
- `src/flywheel/task.py:21-32` — `RubricGrader`: the dataclass that gains two new fields.
- `src/flywheel/loaders.py:98-101` — rubric parsing: the seat for the new field plumbing.
- `_schema/persistence-schema.sql:73-75` — the existing rubric payload shape contract.
- `docs/vision.md:130-149` — the "Graders" section that loses the rubric-pauses-for-review statement.
- `https://www.anthropic.com/engineering/demystifying-evals-for-ai-agents` — the LLM-as-judge pattern the verdict envelope and "unknown" escape hatch are modeled on.

## Decisions Log

| Decision                                          | Choice                                                                                                                              | Rationale                                                                                                                                                                                                                                                                          |
| ------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Feedback storage                                  | Reuse `GraderResultRecord.payload`; harness reads via `store.list_grader_results` and passes findings into `IterationInputs`.       | Single source of truth (the receipt is already required for audit). `flywheel.prompt` stays pure — it receives a `tuple[RubricFindings, ...]`, not a store reference. No schema migration (payload shape already documented).                                                      |
| Policy scope                                      | Per-grader `retry_on_fail: bool = True` on `RubricGrader`.                                                                          | The schema declares behavior; the harness reads it at validation time. Symmetric with how command/transcript graders already imply retry. Different rubrics in the same task can have different policies (e.g. semantic correctness retries, style nit halts for review).        |
| Judge model selection                             | Optional `RubricGrader.judge_model` overrides `HarnessConfig.rubric_judge_model` default.                                           | Per-grader intent expressible in the schema; operator sets the default per worker. Mirrors the existing `model` plumbing in `workflow.py`.                                                                                                                                         |
| Judge isolation                                   | Fresh `claude_agent_sdk.query` session (no `session_id` inheritance) with cwd = attempt's worktree.                                 | Prevents the judge from being biased by the working agent's internal reasoning. Matches the "untrusted claim" principle in `docs/vision.md`.                                                                                                                                       |
| Judge tools                                       | Same full tool surface as the working agent.                                                                                        | Lets the judge run `git diff`, inspect files, run tests — the "grounded in tool results" pattern the Anthropic blog endorses. Documented risk: judge can mutate the worktree. v1 trusts the prompt; future hardening may restrict tools if mutation pollutes downstream graders. |
| Judge budget                                      | `HarnessConfig.rubric_judge_max_turns: int = 8`. No token cap (SDK context window is the ceiling).                                  | Generous-but-bounded default. No-tool verdicts finish in 1 turn; cap is headroom for tool-using judgments. Prevents infinite runaway.                                                                                                                                              |
| Judge evidence                                    | Goal + assertions + working agent's transcript. Judge inspects diff/files via its own tools.                                        | Matches the Anthropic blog: "judge sees the transcript." Judge with tools doesn't need pre-computed diff — it can read what it needs. Simpler runner (pure prompt assembly).                                                                                                       |
| Verdict envelope                                  | Fenced JSON block `<!-- RUBRIC_VERDICT -->` carrying `{passed, summary, unknown?}`.                                                 | Reuses the LOOP_STATUS envelope discipline already proven for working-agent envelopes. Minimal shape (no per-assertion array) — the summary is what the next-iteration prompt needs. Malformed verdicts route through INTERNAL_ERROR.                                              |
| Verdict parser location                           | Inside `grader_rubric.py`, mirroring `flywheel.envelope` discipline.                                                                | Keeps `flywheel.envelope` focused on the working-agent contract. Verdict parser is pure relative to the SDK call.                                                                                                                                                                  |
| Unknown verdict                                   | Treat as pass; emit `harness.rubric_unknown` event.                                                                                 | The Anthropic blog's "give the judge a way out" pattern. Avoids retry storms when the judge legitimately lacks evidence. Operator-observable via the event log.                                                                                                                    |
| Retry budget                                      | Rubric retries share `max_retries` with command/transcript via the existing `FAILED_VALIDATION` source state.                       | One operator knob, one mental model. The existing `is_retry_eligible` check is unchanged.                                                                                                                                                                                          |
| Retries-exhausted terminal                        | `FAILED_VALIDATION -> FAILED` with the rubric grader name and findings summary in `lifecycle.error`.                                | Same terminal path as any other exhausted-retry failure. Findings remain queryable via `GraderResultRecord` for forensic work.                                                                                                                                                     |
| Judge-infrastructure failure                      | `INTERNAL_ERROR` (retry-eligible under existing budget).                                                                            | Judge SDK failure is infrastructure, not validation. Mirrors the agent-crash classification at `harness.py:622-637`. Distinguishes "judge said fail" from "judge couldn't say anything."                                                                                          |
| `retry_on_fail=False` exit transition             | `VALIDATING -> INTERRUPTED` (lifecycle parks for operator review).                                                                  | Preserves the original `docs/vision.md:132` behavior as an opt-in for high-stakes rubrics (e.g., "no security regressions"). Operator can resume via existing manual path or future `recheck-blocked` primitive once spec 00004 lands.                                            |
| Multiple rubric graders per task                  | Each `RubricGrader` runs once per Attempt, in `task.graders` order, first-fail short-circuits within the type.                      | Matches the existing within-type cost-order rule for command graders. No new ordinal semantics needed.                                                                                                                                                                             |
| Schema for receipts                               | Reuse the existing `grader_results.payload_json` shape documented at `_schema/persistence-schema.sql:73`.                           | No migration required. The runner emits a payload matching the documented `{judge_model, judge_model_version, prompt_path, artifacts, per_assertion, usage}` keys — though `per_assertion` is left empty in v1 (no per-assertion verdict), and `prompt_path` may be omitted.       |
| `RubricFindings` placement                        | Pure dataclass; lives in either `flywheel.prompt` or `flywheel.grader_rubric` per implementer judgment.                             | Must be importable by `flywheel.harness` (to construct the tuple) and `flywheel.prompt` (to render). If placed in `prompt.py`, `harness.py` imports it from there; if in `grader_rubric.py`, both import from there. Either preserves purity invariants.                          |
| Documentation update                              | Retract `docs/vision.md:132`; add rubric to the `docs/vision.md:149` auto-retry list; document the verdict envelope in `docs/loop.md`. | The feature changes a documented MVP behavior; the docs must move with the code.                                                                                                                                                                                                   |

## Open Questions

None remaining for v1. Items deferred to follow-ups:

- A per-assertion verdict shape (judge returns one verdict per assertion). The Anthropic blog mildly endorses "isolated evaluation dimensions"; v1 keeps a single summary because that's all the next-iteration prompt needs. Add `per_assertion` rendering once we have evidence the working agent benefits from finer-grained feedback.
- Restricting the judge's tool surface (Read/Grep/Glob only) if mutation pollutes downstream graders in practice. Captured as a non-functional risk in the security section.
- `ManualGrader` execution and the manual approval surface (lifecycle pause + operator decision verbs). Separate spec; this one is rubric-only.
- A `rubric_judge_max_total_tokens` cap on `HarnessConfig` if the turn cap proves insufficient for runaway control.
- Lifting the rubric-judge invocation into a background task / async worker if rubric latency becomes the lifecycle bottleneck. Today the rubric runs inline within `_validate`.
- Per-assertion `weight` on `RubricGrader` (e.g., "this assertion is mandatory; this one is advisory") once we observe how rubrics are written in practice.

## Next Steps

Run `/task 00005-FEATURE-rubric-grader-and-feedback-retry` to generate implementation tasks from this spec.
