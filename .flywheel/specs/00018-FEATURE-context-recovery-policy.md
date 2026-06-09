# Feature: Context-Recovery Policy (Summarize-Restart)

## Summary

When a running attempt approaches the agent's context-window capacity, the harness recovers the run instead of letting it grind to a degraded halt: it force-finalizes the current attempt, produces a structured handoff summary from a fresh context, and schedules a new attempt seeded with that summary. This converts the loop's existing context-pressure *detection* into a first-class recovery *action*, closing the `"context-recovery policy"` gap in `_DEFERRED_LOOP_SUBSYSTEMS`.

## Background

`docs/vision.md` makes "context is a finite resource with diminishing returns" a core principle, and the safety net (spec 00016) ships detection for runaway/thrash conditions — but every detector's only move is to **halt** (`internal_error` / `interrupted` / `failed_validation`). Nothing recovers a run that is simply running out of room. The phase 18+19 audit (`.workflow/audits/19-in-loop-verification-gate.md`, finding 3) showed single attempts burning 6.4M tokens; the loop can see and stop such a run but cannot save it.

Context-pressure telemetry (spec 00009) deliberately stops at raw signals: it emits per-iteration token breakdowns but **no derived utilization% and no threshold crossings**, because the SDK provides no context-window-capacity source. Recovery therefore cannot read an existing "90%" signal — it must own the capacity input, exactly as the hang watchdog (spec 00016) owns its threshold ("mechanism shipped, value operator-supplied until research grounds one", `harness.py:47`). This spec adopts that same pattern.

This is the natural completion of the safety-net arc (detect -> act) and the single most-cited open gap in the loop.

## Scope

### In Scope

- A **summarize-restart** recovery action: force-finalize the current attempt, generate a structured handoff summary from a fresh (non-working-agent) context, and schedule a new attempt whose prompt carries that summary.
- An **operator-supplied capacity trigger**: `HarnessConfig.context_window_tokens` (capacity) + `context_recovery_trigger_ratio` (default `0.9`). Recovery is **disabled** when `context_window_tokens is None`, so existing consumers see no behavior change.
- A new terminal-for-the-attempt `Outcome.RECOVERED` recording that the attempt ended because the harness reset context — neither success, validation failure, nor agent error.
- A recovery **budget**, `max_context_recoveries` (default `1`), separate from the validation-retry budget (`max_retries`). When exhausted, recovery is skipped and the iteration loop follows its normal termination path.
- A `harness.context_recovery` audit event recording occupancy, capacity, ratio, budget usage, and a summary digest.
- A `# Recovery handoff` prompt section on the recovery attempt, mirroring the existing `# Reviewer feedback` section.
- An injected summarizer seam (`recovery_summarizer_invoke`) mirroring the rubric `rubric_judge_invoke` test seam, so the summarizer is driven by a scripted invoker in tests and a fresh SDK query in production.

### Out of Scope

- **Other recovery actions** — `compact` (in-place context trimming) and `fork` (branch the attempt). Action *selection* policy is deferred; this spec hardwires summarize-restart.
- **Threshold/ratio tuning and capacity auto-detection.** Capacity is operator-supplied; deriving it from the SDK or grounding a default ratio is deferred.
- **Intra-iteration (mid-turn) recovery.** Recovery is evaluated only at iteration boundaries (after `harness.iteration_completed`). Saving a single in-flight long iteration depends on streaming usage, which is a separate gap (audit finding 3).
- **Operator-initiated recovery** (a `recover` control verb). MVP recovery is automatic only; no new `CONTROL_COMMAND_*`.
- **Cross-process recovery-budget persistence.** The budget counter is in-process for the `run_task` retry loop; a process that dies mid-run resets it. A persistent counter is deferred.
- **Auto-generating the in-loop-verification fixture body.** This feature trips a loop-path signal (see Decisions Log), so `/task` auto-requires an `in-loop-verification` slot, but the test body is authored by hand — only the slot is auto-required.

## Requirements

### Functional Requirements

1. **FR-1: Capacity-ratio trigger.** After an iteration completes and the run would otherwise continue, the harness compares the iteration's input-side context occupancy (input + cache-read + cache-creation tokens, from the same usage breakdown emitted on `harness.iteration_completed`) against `context_window_tokens`. When `occupancy / context_window_tokens >= context_recovery_trigger_ratio` and recovery budget remains, recovery fires.
   - Acceptance: a harness test with `context_window_tokens` set and a scripted iteration whose usage crosses the ratio finalizes the attempt with `Outcome.RECOVERED` and schedules a new attempt; an iteration below the ratio does not.

2. **FR-2: Disabled by default.** When `context_window_tokens is None`, no occupancy is computed and recovery never fires; the harness behaves exactly as today.
   - Acceptance: the existing harness suite passes unchanged; a test asserts an over-capacity-magnitude usage with `context_window_tokens=None` produces no recovery and no `harness.context_recovery` event.

3. **FR-3: Summarize-restart action.** On trigger, the harness (a) force-finalizes the current attempt with `Outcome.RECOVERED`, (b) invokes the summarizer to produce a structured handoff (work done, work remaining, key decisions, suggested next step) from the task goal, the cumulative diff/artifacts, and the recent transcript tail, and (c) schedules a new attempt whose prompt includes a `# Recovery handoff` section rendering that summary.
   - Acceptance: a test drives a recovery and asserts the next attempt's prompt contains the `# Recovery handoff` section with the summarizer's content; the summarizer is reached via the injected seam, not the working-agent invoker.

4. **FR-4: Recovery budget.** At most `max_context_recoveries` recoveries occur per `run_task` call (default `1`). Once exhausted, a subsequent over-ratio iteration does **not** recover; the iteration loop proceeds to its normal termination (validation or terminal outcome).
   - Acceptance: with `max_context_recoveries=1`, a run whose every iteration is over-ratio recovers exactly once, then completes its second attempt through the normal path; a `harness.context_recovery` event is emitted exactly once.

5. **FR-5: Recovery audit event.** Each recovery emits a `harness.context_recovery` event carrying `iteration`, `attempt_number`, occupancy tokens, `context_window_tokens`, `context_recovery_trigger_ratio`, `recoveries_used`, `recoveries_remaining`, and a digest (length and/or hash) of the handoff summary.
   - Acceptance: the audit stream for a recovered run contains one `harness.context_recovery` event with those fields, ordered before the recovery attempt's `AttemptStarted`.

6. **FR-6: Precedence.** Within one iteration, an agent completion claim and a safety-net (`LoopGuard`) STUCK/THRASH verdict both take precedence over recovery — a run that is done gets validated, and a thrashing run gets halted, neither gets handed more budget. Recovery fires only for a run that is otherwise validly continuing (`intent=continue`) and not flagged by the loop guard.
   - Acceptance: tests assert that (a) a `LoopGuard` verdict + over-ratio iteration halts (no recovery), and (b) a completion-claim + over-ratio iteration validates (no recovery).

7. **FR-7 (loop-path verification):** An `in-loop-verification`-tagged task drives the real `orchestrate` loop with a scripted invoker through a full summarize-restart and asserts the shipped path executes end-to-end: `Outcome.RECOVERED` recorded, recovery attempt started, `# Recovery handoff` in the second prompt, `harness.context_recovery` event persisted.
   - Acceptance: the task's command graders run the real loop (not a unit stub) and pass on the integrated path.

### Non-Functional Requirements

- **Performance**: the occupancy check is O(1) over the just-emitted usage payload and runs once per iteration; no new per-iteration cost. The summarizer call happens only on an actual recovery.
- **Security**: the handoff summary may contain prompt/diff content; it is persisted in the audit stream like all other payloads and subject to the existing read-time `Redactor`. No new sensitive sink.
- **Purity**: no `flywheel.task` / `flywheel.lifecycle` purity violation — the new `Outcome.RECOVERED` member is a plain enum entry; summarization and IO live in the harness/invoker layer.

## Behavior Specification

### Happy Path

1. An attempt runs; an iteration completes and emits `harness.iteration_completed` with its usage breakdown.
2. The run would continue (`intent=continue`, no loop-guard verdict, no completion claim) and recovery budget remains.
3. Occupancy / capacity crosses `context_recovery_trigger_ratio`.
4. The harness finalizes the attempt with `Outcome.RECOVERED` and emits `harness.context_recovery`.
5. The summarizer (fresh context) produces a structured handoff from goal + diff + transcript tail.
6. A new attempt is scheduled (`RetryScheduled` -> `AttemptStarted`), its prompt carrying `# Recovery handoff`.
7. The agent resumes against a fresh context with the summary and continues toward `done`.

### Error Handling

| Error Condition | Expected Behavior |
| --------------- | ----------------- |
| Summarizer invoke raises / times out | Recovery is aborted for this iteration; route through `INTERNAL_ERROR` (same class as a failed rubric judge). Do not silently restart with an empty handoff. |
| Iteration produced no usage data (occupancy unknown) | Treat as below-ratio: no recovery. The event is not emitted. |
| `context_recovery_trigger_ratio` misconfigured (<=0 or >1) | Reject at config construction with a clear error; do not clamp silently. |
| `context_window_tokens <= 0` | Treated as misconfiguration; reject at config construction. |

### Edge Cases

| Case | Expected Behavior |
| ---- | ----------------- |
| Over-ratio on the very first iteration | Recover normally; first attempt finalizes `RECOVERED`, second attempt is the working attempt. |
| Budget exhausted, still over-ratio | No recovery; normal termination path. No `harness.context_recovery` event. |
| `max_iterations_per_attempt=1` (default) and a single over-ratio iteration | Recovery applies between attempts: the completed iteration's attempt finalizes `RECOVERED` and the next attempt starts fresh with the handoff. (Mid-turn rescue is out of scope.) |
| Recovery + validation-retry interplay | Recovery consumes only `max_context_recoveries`; validation failures consume only `max_retries`. The two budgets are independent. |
| Capacity set but ratio never crossed | Feature is inert for that run; identical to disabled behavior. |
| Process dies mid-run | In-process recovery counter is lost (documented MVP limitation); on resume the run may recover up to `max_context_recoveries` again. |

## Technical Context

### Affected Apps

- `src/flywheel` (single package): `lifecycle.py` (new `Outcome.RECOVERED`), `harness.py` (config fields, trigger evaluation, summarize-restart orchestration, event emission), `prompt.py` (`# Recovery handoff` section), a summarizer runner (new `recovery_summarizer.py` or a function alongside the rubric runner), `events.py` (only if a typed event is preferred over a kind-string `_emit`; default is kind-string, no dataclass change).

### Integration Points

- **Trigger evaluation** sits in the iteration loop immediately after the `loop_guard.observe(...)` verdict check (`harness.py` ~`2492`), where `intent=continue` is decided. Both are "preempt-continue" verdicts; loop-guard precedence (FR-6) is enforced by ordering the loop-guard check first.
- **Usage source** reuses `usage_payload` already built for `harness.iteration_completed` (spec 00009) — no new SDK plumbing.
- **Summarizer seam** mirrors `HarnessConfig.rubric_judge_invoke` / `JudgeInvoke` (`harness.py:~325`): a `recovery_summarizer_invoke` config field, `None` in production (fresh `claude_agent_sdk.query`), scripted in tests.
- **Retry/attempt machinery** reuses `RetryScheduled` -> `AttemptStarted` and the prompt-feedback rendering arm (`prompt.py:_section_reviewer_feedback` is the structural precedent for `_section_recovery_handoff`).
- **Outcome persistence** writes the new enum string into the existing `attempts.outcome` column — no `ADD COLUMN`.

### Relevant Existing Code

- `src/flywheel/harness.py:154` — `_DEFERRED_LOOP_SUBSYSTEMS` (`"context-recovery policy"`); remove this entry when shipped.
- `src/flywheel/harness.py:~2447-2505` — `harness.iteration_completed` emission and the `loop_guard.observe` / `intent=continue` decision point.
- `src/flywheel/lifecycle.py:20` — `Outcome` enum.
- `src/flywheel/prompt.py:168` — `_section_reviewer_feedback`, the feedback-section precedent.
- `.workflow/specs/00009-FEATURE-context-pressure-telemetry.md` — why no utilization% exists upstream.
- `.workflow/specs/00016-FEATURE-loop-safety-net.md` — the detect-only precedent and operator-supplied-threshold pattern.

## Decisions Log

| Decision | Choice | Rationale |
| -------- | ------ | --------- |
| Trigger source | Operator-supplied `context_window_tokens` + `context_recovery_trigger_ratio` (default `0.9`); disabled when capacity is `None` | Telemetry (00009) emits no utilization% and the SDK exposes no capacity; mirrors the hang-watchdog "mechanism shipped, value operator-supplied" pattern (00016). Off-by-default preserves existing behavior. |
| Occupancy metric | Latest iteration's input-side tokens (input + cache-read + cache-creation), not a summed running counter | That single value *is* the current context sent to the model; summing per-iteration deltas double-counts the re-sent conversation. Also sidesteps a cross-process counter. |
| Recovery action | Summarize-restart only | Most general and reuses the fresh-context retry + feedback-prompt arm; `compact`/`fork` are the deferred action-selection scope. |
| Summary producer | Separate summarizer invoke (fresh context, injected seam), not the working agent | The working agent is near capacity and degraded; a distinct call mirrors the rubric "separate LLM, distinct from the working agent" philosophy and is deterministically testable. |
| Restart vehicle | New attempt via `RetryScheduled`/`AttemptStarted` with a `# Recovery handoff` prompt section | Reuses shipped retry-with-feedback machinery; a fresh attempt is a fresh context, which is the point of summarize-restart. Live-inject/compact-in-place is the deferred `compact` action. |
| Outcome model | New `Outcome.RECOVERED`; no new `Status` | The attempt ended by harness-initiated reset — honestly neither success nor failure. Lifecycle stays within existing statuses (`READY`/`RUNNING`) via the retry arm. |
| Budget | Separate `max_context_recoveries` (default `1`), in-process | Recovery is a distinct failure class from validation retry; bounding prevents infinite restart. In-process is correct for the single `run_task` retry loop; cross-process persistence deferred. |
| Auditability of recovery count | Derive from in-process counter + `harness.context_recovery` events; no schema column | Avoids an `ADD COLUMN` (loop-path signal 2) and a new store Protocol read method (signal 4); keeps loop-path footprint to signal 1 only. |
| Event type | Kind-string `harness.context_recovery` via `_emit`, not an `events.py` dataclass | Consistent with other `harness.*` operational events; no domain-event/schema churn. |
| Precedence | Completion-claim > loop-guard halt > recovery | A done run is validated; a thrashing run is halted; only a validly-continuing run is handed a fresh context. |
| Loop-path coverage | Required | Spec trips signal 1 (new `Outcome.RECOVERED` member in `lifecycle.py`). `/task` must emit an `in-loop-verification` slot (FR-7) driving the real `orchestrate` loop with a scripted invoker. No schema migration, so FR-4's `v(N-1)` store-seeding clause does not apply. |

## Open Questions

None blocking. Two items are deliberately deferred, not unresolved: a grounded default for `context_recovery_trigger_ratio` (currently `0.9` by convention) and capacity auto-detection — both belong to the deferred tuning scope.

## Next Steps

Run `/task 00018-FEATURE-context-recovery-policy` to generate implementation tasks from this spec. The task set must include an `in-loop-verification`-tagged slot (FR-7) that drives the real `orchestrate` loop through a full summarize-restart.
