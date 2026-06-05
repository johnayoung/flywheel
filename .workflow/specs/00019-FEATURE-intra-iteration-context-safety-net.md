# Feature: Intra-Iteration Context Safety Net

## Summary

Give the harness a context-occupancy decision point *inside* a long single SDK iteration, not only at iteration boundaries. The harness tracks running input-side occupancy mid-turn, emits `harness.context_threshold_crossed` events at 50/75/90%, and at the configured recovery ratio interrupts the in-flight iteration to fire the existing summarize-restart recovery (spec 00018) — closing the blind spot where a single iteration burns millions of tokens with zero harness intervention.

## Background

`docs/vision.md` makes runaway burn the headline problem ("it can loop, consume context, burn budget"). The safety-net arc shipped detection and recovery — hang watchdog and thrash detection (spec 00016), context-recovery summarize-restart (spec 00018) — but **every one of those controls only fires at an iteration boundary** (after `harness.iteration_completed`). With the harness default `max_iterations_per_attempt=1`, a one-shot task produces exactly one boundary, so a long single iteration is a complete blackout for the safety net.

The last two phase audits quantify the gap:

- `.workflow/audits/19-in-loop-verification-gate.md` finding 3 — a single attempt burning 6.4M tokens.
- `.workflow/audits/20-context-recovery-policy.md` — two tasks each burned ~14.6M tokens / $6.54-$6.78 across ~70 SDK turns *within one harness iteration*. The audit's own diagnosis: "the loop did not misfire; it has no mid-turn visibility into a long single iteration," and lists "intra-iteration recovery depends on streaming usage" as the explicit out-of-scope gap of spec 00018.

This feature converts the mid-turn blackout into a decision point, activating the recovery the safety net already knows how to perform.

**Feasibility (settled during `/define`):** the original open risk — whether streamed messages carry usage or whether usage only lands on the terminal `ResultMessage` — resolved favorably:

- `claude_agent_sdk.AssistantMessage.usage` exists (`types.py:1032`, `dict[str, Any] | None`). Every streamed assistant message carries per-turn usage, observable in the existing `on_message` tap.
- `ClaudeSDKClient.get_context_usage()` returns `ContextUsageResponse` (`types.py:759`) with `totalTokens`, `maxTokens` (capacity), `rawMaxTokens`, and a computed `percentage` (0-100) — a direct SDK source for both occupancy and capacity that spec 00018 assumed did not exist.
- The production invoker `invoke_iteration_with_client` (`invoker_client.py`) already runs a live `ClaudeSDKClient` with a polling watcher coroutine, so `get_context_usage()` can be called from a loop that already exists.

## Scope

### In Scope

- **Mid-turn occupancy tracking** fed by the existing `on_message` seam, reusing the watchdog wrapper at `harness.py:2586`. The harness accumulates input-side occupancy from streamed `AssistantMessage.usage` as the always-available baseline signal, and opportunistically reads `ClaudeSDKClient.get_context_usage()` for exact occupancy + capacity when the live client exposes it (hybrid).
- **Hybrid capacity source.** Capacity (the ratio denominator) is taken from the SDK's `get_context_usage().maxTokens` / `percentage` when available, falling back to operator-supplied `HarnessConfig.context_window_tokens` (added in spec 00018) otherwise. The feature stays **off by default**: when no capacity is available from either source, no occupancy is computed and nothing fires.
- **Tiered observe events.** `harness.context_threshold_crossed` emitted at fixed 50%, 75%, and 90% occupancy, each at most once per iteration (a crossing is the first message that pushes occupancy at or above the tier).
- **Mid-turn act.** When occupancy crosses `context_recovery_trigger_ratio` (the same `HarnessConfig` field spec 00018 uses at boundaries, default `0.9`) and recovery budget remains, the harness interrupts the in-flight iteration (`ClaudeSDKClient.interrupt`, adopted in spec 00013) and routes into the existing summarize-restart recovery: finalize the attempt `Outcome.RECOVERED`, summarize, schedule a fresh attempt with the `# Recovery handoff` prompt section.
- **Shared recovery budget.** Mid-turn and iteration-boundary recovery draw from the same `max_context_recoveries` counter (default `1`). One recovery is one recovery regardless of where it fires.
- A `harness.context_recovery` audit event for a mid-turn recovery, carrying the same fields spec 00018 emits plus a marker that the trigger was mid-turn (e.g. `trigger="mid_turn"`).

### Out of Scope

- **Thrash and hang mid-turn coverage.** Only context-occupancy gets the mid-turn decision point. The hang watchdog already taps `on_message` for timing; extending LoopGuard thrash (net-diff / input-novelty) to fire mid-turn remains the deferred sub-problem in `_DEFERRED_LOOP_SUBSYSTEMS`.
- **Other recovery actions** (`compact`, `fork`). Mid-turn recovery reuses summarize-restart; action selection stays deferred (spec 00018 scope).
- **Threshold/ratio tuning research.** The 50/75/90 observe tiers and the `0.9` act ratio are conventions; grounding them empirically is deferred. (The SDK's `percentage` makes the *signal* exact even while the *chosen ratio* stays a convention.)
- **Operator-initiated mid-turn recovery** (a `recover` control verb). Mid-turn recovery is automatic only; no new `CONTROL_COMMAND_*`.
- **Cross-process recovery-budget persistence.** The shared counter is in-process for the `run_task` retry loop, inheriting the spec 00018 limitation.
- **Auto-generating the in-loop-verification fixture body.** The slot is auto-required (see Decisions Log); the test body is authored by hand.

## Requirements

### Functional Requirements

1. **FR-1: Mid-turn occupancy tracking.** During an iteration, the harness updates a running input-side occupancy estimate from each streamed `AssistantMessage.usage` via the existing `on_message` tap, reusing the `_occupancy_from_usage` input-side definition (`input + cache_read + cache_creation`, `harness.py:504`). When the live client exposes `get_context_usage()`, its `totalTokens` is preferred over the accumulated estimate for that check.
   - Acceptance: a harness test streams scripted `AssistantMessage`s with rising `usage` and asserts the tracked occupancy follows them mid-iteration (before any `harness.iteration_completed`).

2. **FR-2: Off by default.** When neither the SDK nor `context_window_tokens` yields a capacity, no occupancy ratio is computed, no threshold events are emitted, and no mid-turn act fires. The existing harness suite passes unchanged.
   - Acceptance: a test with capacity unavailable streams over-magnitude usage and asserts no `harness.context_threshold_crossed`, no `harness.context_recovery`, and identical control flow to today.

3. **FR-3: Tiered observe events.** With capacity available, the harness emits `harness.context_threshold_crossed` at the first mid-turn point occupancy reaches 50%, 75%, and 90%, each at most once per iteration, carrying `iteration`, `tier`, occupancy tokens, capacity, computed `percentage`, and the capacity source (`sdk` / `operator`).
   - Acceptance: a test streaming usage that crosses all three tiers produces exactly three events in tier order; a re-cross of an already-emitted tier in the same iteration produces no duplicate.

4. **FR-4: Mid-turn act → summarize-restart.** When occupancy crosses `context_recovery_trigger_ratio` and recovery budget remains, the harness interrupts the in-flight iteration and routes into the spec 00018 recovery path: the attempt finalizes `Outcome.RECOVERED`, the summarizer (fresh context, injected seam) produces a handoff, and a new attempt is scheduled with the `# Recovery handoff` prompt section.
   - Acceptance: an integrated test drives an iteration whose streamed usage crosses the ratio and asserts the iteration is interrupted, the attempt is `Outcome.RECOVERED`, and the next attempt's prompt contains `# Recovery handoff` from the summarizer seam.

5. **FR-5: Shared recovery budget.** Mid-turn and boundary recovery decrement the same `max_context_recoveries` counter. With `max_context_recoveries=1`, a run that recovers mid-turn cannot also recover at a later boundary (and vice versa).
   - Acceptance: a test with `max_context_recoveries=1` and both a mid-turn over-ratio iteration and a later over-ratio boundary asserts exactly one `harness.context_recovery` event total.

6. **FR-6: Mid-turn recovery audit event.** A mid-turn recovery emits one `harness.context_recovery` event with the spec 00018 fields plus `trigger="mid_turn"`, ordered before the recovery attempt's `AttemptStarted`.
   - Acceptance: the audit stream for a mid-turn-recovered run contains one such event with `trigger="mid_turn"` in the correct order.

7. **FR-7: Precedence and no double-fire.** A mid-turn recovery and the existing boundary recovery check (spec 00018 FR-1) must not both fire for the same crossing: once an iteration is interrupted for mid-turn recovery, the boundary check does not additionally recover. Operator interrupt / SIGTERM still takes precedence (routes through `_handle_interrupt`, not recovery), mirroring the hang watchdog's `_HangDetected` vs external-cancel distinction (`harness.py:2569`).
   - Acceptance: tests assert (a) a mid-turn recovery does not also trigger a boundary recovery on the same attempt, and (b) an external cancel during an over-ratio iteration routes to `interrupted`, not recovery.

8. **FR-8 (loop-path verification):** An `in-loop-verification`-tagged task drives the real `orchestrate` loop with a scripted invoker through a full mid-turn recovery and asserts the shipped path executes end-to-end: occupancy crosses the ratio mid-iteration, the iteration is interrupted, `Outcome.RECOVERED` is recorded, the recovery attempt starts with `# Recovery handoff`, and a `harness.context_recovery` event with `trigger="mid_turn"` is persisted.
   - Acceptance: the task's command graders run the real loop (not a unit stub) and pass on the integrated mid-turn path.

### Non-Functional Requirements

- **Performance**: occupancy accumulation is O(1) per streamed message over the just-arrived usage payload; reuses the watchdog's existing subscription, adding no second `on_message`. `get_context_usage()` is read at most once per watcher poll interval (not per message), only when the live client is in use.
- **Security**: threshold and recovery events carry token counts and the handoff digest only; no new sensitive sink beyond what spec 00018 already persists, subject to the existing read-time `Redactor`.
- **Purity**: `flywheel.task` / `flywheel.lifecycle` stay pure — no new enum member is added (mid-turn recovery reuses `Outcome.RECOVERED`); all occupancy/interrupt/IO logic lives in `harness.py` / `invoker_client.py`.

## Behavior Specification

### Happy Path

1. An iteration runs; the agent streams `AssistantMessage`s, each carrying `usage`.
2. The harness's occupancy tap accumulates input-side tokens (or reads exact `get_context_usage().totalTokens` when the live client provides it) and divides by capacity (SDK `maxTokens` or operator `context_window_tokens`).
3. Occupancy crosses 50%, then 75% → a `harness.context_threshold_crossed` event fires at each tier.
4. Occupancy crosses `context_recovery_trigger_ratio` (0.9) with budget remaining → the harness interrupts the in-flight iteration.
5. The interrupted attempt finalizes `Outcome.RECOVERED`; `harness.context_recovery` (`trigger="mid_turn"`) is emitted.
6. The summarizer (fresh context) produces a handoff; a new attempt is scheduled with `# Recovery handoff`.
7. The agent resumes against a fresh context and continues toward `done`.

### Error Handling

| Error Condition | Expected Behavior |
| --------------- | ----------------- |
| `get_context_usage()` raises / unsupported by the client | Fall back to the accumulated `AssistantMessage.usage` estimate; no error surfaced. |
| Streamed messages carry no usage and no SDK reading available | Occupancy stays 0 (treated as below-threshold); no events, no act. Same as the spec 00018 "no usage data" case. |
| Summarizer invoke raises / times out during mid-turn recovery | Recovery aborted for this crossing; route through `INTERNAL_ERROR` (same class as a failed boundary recovery / rubric judge). Do not restart with an empty handoff. |
| Interrupt races with normal iteration completion | If the iteration completes before the interrupt lands, honor the completion and fall through to the boundary check (which then sees the same budget); finalize exactly once (mirrors the watchdog race handling at `harness.py:2575`). |
| `context_recovery_trigger_ratio` / `context_window_tokens` misconfigured | Already rejected at config construction by spec 00018; no new validation needed. |

### Edge Cases

| Case | Expected Behavior |
| ---- | ----------------- |
| Capacity available only from operator knob (plain `query` path, no client) | Mid-turn act still works via `AssistantMessage.usage` accumulation + operator capacity; observe events use source `operator`. The interrupt path requires the client — on the plain path, mid-turn *act* degrades to observe-only (documented), since there is no `interrupt` to call. |
| SDK `percentage` and accumulated estimate disagree | SDK reading wins when present; the estimate is only the fallback. |
| Crossing 90% and the act ratio on the same message | Emit the 90% observe event, then act; the event is ordered before `harness.context_recovery`. |
| Budget already exhausted when ratio crosses mid-turn | No interrupt, no recovery; observe events still fire. The iteration runs to its natural end. |
| Over-ratio on the first streamed message of the first iteration | Recover normally; first attempt finalizes `RECOVERED`, second attempt is the working attempt. |
| Mid-turn recovery + validation-retry interplay | Mid-turn recovery consumes only `max_context_recoveries`; validation failures consume only `max_retries`. Budgets stay independent (spec 00018). |

## Technical Context

### Affected Apps

- `src/flywheel` (single package):
  - `harness.py` — mid-turn occupancy accumulation in the `on_message`/watchdog wrapper, tiered `harness.context_threshold_crossed` emission, mid-turn trigger evaluation, interrupt-to-recovery routing reusing the spec 00018 recovery orchestration and shared budget. Reuses `_occupancy_from_usage` (`harness.py:504`) and `total_tokens_from_usage` (`grader_transcript.py:72`).
  - `invoker_client.py` — expose a hook to read `get_context_usage()` from the existing watcher loop and to interrupt the in-flight iteration for a harness-initiated mid-turn recovery (distinct from the operator `interrupt` control command and from external cancel).
  - `events.py` — only if a typed event is preferred over a kind-string `_emit`; default is kind-string `harness.context_threshold_crossed`, no dataclass change.

### Integration Points

- **Occupancy tap** reuses the watchdog wrapper at `harness.py:2586` (`watchdog_on_message`), where every SDK message already flows mid-iteration — no second subscription.
- **Exact occupancy + capacity** via `ClaudeSDKClient.get_context_usage()` (`client.py:506`), read from the watcher loop in `invoke_iteration_with_client` (`invoker_client.py:~330`).
- **Mid-turn interrupt** reuses `ClaudeSDKClient.interrupt` (`client.py:313`), the mechanism spec 00013 adopted; the harness-initiated cancel must be distinguishable from operator-interrupt and external-cancel (mirror the `_HangDetected` vs external-`CancelledError` split at `harness.py:2569`).
- **Recovery orchestration, budget, summarizer seam, and `# Recovery handoff`** are all reused verbatim from spec 00018 (`recovery_summarizer.py`, `prompt.py:_section_recovery_handoff`, the `_RecoveryState` / `_RecoveryTrigger` machinery in `_drive_iterations`).

### Relevant Existing Code

- `src/flywheel/harness.py:504` — `_occupancy_from_usage`, the input-side occupancy definition to reuse.
- `src/flywheel/harness.py:2541-2667` — `_invoke_with_watchdog`, the existing mid-iteration `on_message` tap and the cancel/race pattern to mirror.
- `src/flywheel/harness.py:2728-2837` — `_drive_iterations`, the iteration loop, `harness.iteration_completed` emission, and the boundary recovery trigger (spec 00018 FR-1).
- `src/flywheel/invoker_client.py:~300-375` — `invoke_iteration_with_client`, the live-client watcher loop where `get_context_usage()` and mid-turn interrupt attach.
- `.venv/.../claude_agent_sdk/types.py:759` (`ContextUsageResponse`), `:1024` (`AssistantMessage.usage`) — the SDK signal sources.
- `.workflow/specs/00018-FEATURE-context-recovery-policy.md` — the boundary recovery this feature extends mid-turn.
- `.workflow/specs/00016-FEATURE-loop-safety-net.md` — the watchdog/operator-supplied-threshold precedent.
- `.workflow/audits/20-context-recovery-policy.md` — the quantified blind spot motivating this feature.

## Decisions Log

| Decision | Choice | Rationale |
| -------- | ------ | --------- |
| Signal source | Hybrid: accumulate `AssistantMessage.usage` baseline + opportunistic `get_context_usage()` | Baseline works on every invoker path with no new SDK call; exact SDK reading grounds the signal where the live client allows. Broadest coverage, degrades gracefully. |
| Capacity source | SDK `maxTokens`/`percentage` when available, operator `context_window_tokens` fallback | The SDK exposes real capacity (discovered in `/define`), finally grounding the "value ungrounded" deferral; the operator knob preserves spec 00018's off-by-default contract on the plain path. |
| MVP action | Observe + act (emit tiers AND mid-turn interrupt → summarize-restart) | Closes the actual $6.78 blind spot end-to-end, not just makes it visible; reuses shipped 00018 + 00013 machinery. |
| Threshold model | Fixed 50/75/90 observe events; act at `context_recovery_trigger_ratio` (0.9) | One source of truth for "when to recover" across boundary and mid-turn; tiered observe gives early-warning legibility before the act point. |
| Detector scope | Context-recovery only | Hang already taps `on_message` for timing; thrash mid-turn is a separate deferred sub-problem. Keeps blast radius to the occupancy path and directly closes the cited gap. |
| Recovery budget | Shared `max_context_recoveries` across mid-turn and boundary | One recovery is one recovery; prevents a run recovering twice and keeps spec 00018 budget semantics and auditability intact. |
| Outcome model | Reuse `Outcome.RECOVERED`; no new member | The attempt still ended by harness-initiated context reset; mid-turn vs boundary is recorded via the event `trigger` field, not a new enum. Keeps `lifecycle.py` pure and untouched. |
| Event type | Kind-string `harness.context_threshold_crossed` via `_emit` | Consistent with other `harness.*` operational events; no domain-event/schema churn. |
| Plain-path act degradation | On the no-client `query` path, mid-turn *act* degrades to observe-only | `interrupt` only exists on `ClaudeSDKClient`; observe still works via usage accumulation, and the boundary recovery (00018) still covers that path between iterations. |
| Loop-path coverage | Required (slot included on merit) | The mechanical 00017 Trigger Set is not strictly tripped — no new `Outcome` member (reuses `RECOVERED`), no schema/grader/store-protocol/`CONTROL_COMMAND_*` change — but the feature is a harness control-flow change whose mid-turn interrupt→recovery path is verifiable only through the real `orchestrate` loop. `/task` must emit an `in-loop-verification` slot (FR-8). No schema migration, so the `v(N-1)` store-seeding clause does not apply. |

## Open Questions

None blocking. Two items are deliberately deferred, not unresolved: empirical grounding of the 50/75/90 tiers and the `0.9` act ratio, and mid-turn coverage for thrash/hang detection (out of scope here).

## Next Steps

Run `/task 00019-FEATURE-intra-iteration-context-safety-net` to generate implementation tasks from this spec. The task set must include an `in-loop-verification`-tagged slot (FR-8) that drives the real `orchestrate` loop through a full mid-turn summarize-restart with a scripted invoker.
