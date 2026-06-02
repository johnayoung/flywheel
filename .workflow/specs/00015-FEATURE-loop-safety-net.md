# Feature: Loop safety net (stuck / runaway intervention)

## Summary

The loop can now *observe* a confused agent in real time but cannot *act* on
one. This feature adds the three deterministic safety detectors that
`docs/loop.md` flags as TODO and `src/flywheel/harness.py:116`
(`_DEFERRED_LOOP_SUBSYSTEMS`) explicitly stubs out — each wired to a real
lifecycle transition the harness already owns:

| Detector | Signal (deterministic) | Lifecycle action |
| --- | --- | --- |
| **Repeated-failure block** (`blocked_implicit`, mechanical case) | N consecutive identical `(tool_name, sha256(input))` calls whose result `is_error == True` | `running -> interrupted` (blocked, operator-recoverable) |
| **Thrash** (sub-problem (a) only) | An identical `(tool_name, sha256(input))` tuple recurs `>= K` times within a rolling window of `W` tool calls | `running -> validating -> failed_validation` (retry policy decides) |
| **Hang watchdog** | No SDK message of any kind for longer than the threshold, with no rate-limit liveness in flight | `running -> internal_error` (infrastructure class) |

This closes `docs/vision.md`'s headline problem (line 5): "When an agent gets
confused, it can loop, consume context, burn budget, and produce little of
value." Observability told us it was happening; this stops it.

## Background

Every detector here is named, classified, and left unbuilt in the existing
specs and code:

- `docs/loop.md:38-42` — the detection map flags `blocked_implicit` (mechanical
  case "simple"), `thrashing` (sub-problem (a) "deterministic"), and `hanging`
  ("simple mechanism: a watchdog timer reset on every message") as TODO. The
  judgment-heavy sub-problems are deferred (see Out of Scope).
- `src/flywheel/harness.py:114-122` — `_DEFERRED_LOOP_SUBSYSTEMS` lists
  `"thrash detection"`, `"hang threshold defaults"`,
  `"blocked_implicit semantic similarity"` and the module docstring states they
  are "explicitly deferred ... The harness does not paper over them with stub
  heuristics."

The evidence each detector needs already reaches the harness and is then
unused for control:

- `flywheel.invoker.ToolInteraction` (`src/flywheel/invoker.py:50-64`) pairs
  `tool_name`, `tool_input`, and the matching `ToolResultObservation`
  (`is_error`). Its docstring already states: "The harness keys
  `blocked_implicit` on `(tool_name, sha256(input))` and feeds thrash detection
  the same `(tool, input)` tuple."
- `IterationResult.signals.tool_interactions`
  (`src/flywheel/invoker.py:108`) carries the full ordered tuple for each
  completed iteration, available to the harness the instant `invoke_iteration`
  returns (`src/flywheel/harness.py:1888`).
- The `on_message` per-message seam (`src/flywheel/invoker.py:191-199`,
  wired at `src/flywheel/harness.py:1866-1887`) delivers each SDK `Message`
  the instant it arrives — the heartbeat source the hang watchdog resets on.

The prerequisite infrastructure landed in prior specs: per-iteration telemetry
(`00009`), message-granular persistence and the live operator surface
(`00010`/`00011`), and the interrupt path that already finalizes a mid-attempt
cancellation (`00012`/`00013`). The harness owns every transition this feature
fires; nothing here mutates lifecycle state from outside the harness.

## Scope

### In Scope

- New **pure** module `src/flywheel/loop_guard.py` exposing:
  - `LoopGuardConfig` — threshold knobs (all independently disablable).
  - `LoopGuard` — a per-attempt stateful accumulator with one method,
    `observe(interactions: Sequence[ToolInteraction]) -> LoopGuardVerdict | None`,
    fed each iteration's `signals.tool_interactions` in arrival order.
  - `LoopGuardVerdict` — frozen dataclass: `kind` (`STUCK` | `THRASH`),
    a human-readable `reason`, and the offending `tool_name` + input digest.
  - The repeated-failure and thrash detection logic as deterministic functions
    of the observed tuple sequence. No I/O, no time, no randomness — covered by
    a `tests/test_loop_guard_module_purity.py` mirroring the existing purity
    tests.
- Harness wiring of `LoopGuard` into `_drive_iterations`
  (`src/flywheel/harness.py:1816`): construct one `LoopGuard` per attempt,
  feed it each completed iteration's `tool_interactions`, and on a verdict break
  the iteration loop and route to the mapped transition.
- The **hang watchdog** in the harness (asyncio plumbing — not pure, so it lives
  in `harness.py`, which already imports `asyncio`/`time`): race the
  `invoker(request)` call against a watchdog coroutine that tracks a "last
  message at" monotonic timestamp updated from the existing `_on_message` seam.
  On silence past the threshold it records intent, cancels the invoker task, and
  the harness routes the resulting cancellation to `internal_error` —
  distinguished from an operator interrupt by a guard flag (see Behavior).
- Three new `harness.*` audit events emitted through the existing `_emit` ->
  `append_event` seam: `harness.stuck`, `harness.thrash_detected`,
  `harness.hang_detected`. No new domain-event type (see Decisions).
- `LoopGuardConfig` fields added to `HarnessConfig`
  (`src/flywheel/harness.py:219`) so a single run config carries the policy.
- Tests: each detector trips on a crafted `tool_interactions` / message stream
  and lands on the asserted status with the asserted outcome and audit event;
  each disabled detector never trips; the retry budget behaves as specified for
  thrash; the blocked recovery path (`interrupted -> ready`) clears state.
- A short doc touch-up to `docs/loop.md` (remove the three items from the TODO
  framing where now implemented, note what remains deferred) and to the
  `_DEFERRED_LOOP_SUBSYSTEMS` tuple + harness docstring.

### Out of Scope

- **Thrash sub-problems (b) and (c)** — net-zero-diff detection (needs
  filesystem snapshots and a "meaningful change" heuristic) and input-novelty
  scoring (needs a distance metric over structured inputs). Only sub-problem
  (a), literal tuple repetition, ships. (`docs/loop.md:39`.)
- **`blocked_implicit` semantic similarity** — "same question re-asked"
  detection over assistant text. Only the mechanical identical-`(tool, input)`
  failure counter ships. (`docs/loop.md:38`.)
- **A grounded hang-threshold default.** The watchdog mechanism ships, but
  `hang_timeout_seconds` defaults to `None` (disabled). `docs/loop.md:40`
  states the threshold value "requires research informed by extended-thinking
  budgets and telemetry before a default can be set" — we ship no guessed
  number, consistent with `00009`'s refusal to emit a util% without a grounded
  capacity source.
- **Rate-limit-aware ETA suppression.** A `RateLimitEvent` is an SDK message and
  so resets the watchdog timer as liveness (the simple correct behavior). The
  `RateLimitInfo.ResetsAt`-driven ETA computation from `docs/loop.md:41` is a
  follow-up.
- **A dedicated permission-denial `(tool, resource)` counter.** Permission
  denials that surface as `is_error` tool results feed the repeated-failure
  counter; the separate `signals.permission_denials` resource-keyed counter is
  deferred.
- **Crash-classification refinement** (`internal_error` sub-typing) — unchanged;
  remains in `_DEFERRED_LOOP_SUBSYSTEMS`.
- **Cross-attempt detector state.** Each attempt starts a fresh agent context;
  the `LoopGuard` resets per attempt. No persisted accumulator.

## Requirements

### Functional Requirements

1. **FR-1: Repeated-failure block.** When the same `(tool_name, sha256(input))`
   tuple produces `is_error == True` results `repeated_tool_failure_threshold`
   times consecutively (across the attempt's tool calls, spanning iterations),
   the harness aborts the attempt, finalizes it with `Outcome.CANCELLED`,
   emits `harness.stuck`, records a `Blocked` domain event with a synthesized
   `requires_json` describing the failing tool, and transitions
   `running -> interrupted`. The retry budget is **not** consumed (matches the
   explicit-blocked path, `src/flywheel/harness.py:1649-1700`).
   - Acceptance: a stream with three identical failing `Bash` tool calls
     (threshold 3) lands the lifecycle in `interrupted` with one `harness.stuck`
     event whose payload names the tool and digest; `is_retry_eligible` is
     unchanged; an operator transition to `ready` clears `blocked_requires_json`.

2. **FR-2: Thrash detection.** When an identical `(tool_name, sha256(input))`
   tuple recurs `>= thrash_repeat_threshold` times within the trailing
   `thrash_window` tool calls (regardless of `is_error`), the harness aborts the
   attempt, emits `harness.thrash_detected`, finalizes with `Outcome.AGENT_ERROR`,
   and transitions `running -> validating -> failed_validation` — reusing the
   exact shape of the cap-reached agent-error path
   (`src/flywheel/harness.py:1515-1535`). The outer retry arm then applies
   `is_retry_eligible(max_retries)` (`src/flywheel/harness.py:1300`): `ready`
   if retries remain (emitting `harness.retry_scheduled`), else `failed`.
   - Acceptance: a stream repeating one identical successful tool call past the
     threshold within the window lands in `failed_validation`; with
     `max_retries >= 1` the next transition is `ready`; with `max_retries == 0`
     it is `failed`.

3. **FR-3: Hang watchdog.** When `hang_timeout_seconds` is set and no SDK
   message arrives for longer than that interval during an iteration, the
   harness cancels the in-flight invocation, emits `harness.hang_detected`,
   finalizes the attempt with `Outcome.INTERNAL_ERROR`, and transitions
   `running -> internal_error` (the infrastructure class, matching the crash
   path at `src/flywheel/harness.py:1541-1574`). Any message — including a
   `RateLimitEvent` or a `ThinkingBlock`-bearing `AssistantMessage` — resets the
   timer.
   - Acceptance: a message stream that stalls past the threshold lands in
     `internal_error` with one `harness.hang_detected` event; a stream that
     emits a heartbeat message just under the threshold every tick runs to
     normal completion and never trips.

4. **FR-4: Hang vs operator-interrupt disambiguation.** A watchdog-induced
   cancellation routes to `internal_error` (FR-3), never to the operator
   `harness.interrupted` path (`src/flywheel/harness.py:568-642`). An operator
   interrupt with no watchdog trip still routes to `interrupted` as today.
   - Acceptance: a test asserts a watchdog cancel produces `harness.hang_detected`
     + `internal_error` and emits no `harness.interrupted`; a test asserts an
     operator cancel with the watchdog disabled is unchanged from current
     behavior.

5. **FR-5: Independently disablable, off by default where ungrounded.** Each
   threshold disables independently (`None` / `0`). `hang_timeout_seconds`
   defaults to disabled (no grounded value — see Out of Scope). With all
   detectors disabled the harness behaves exactly as today.
   - Acceptance: a regression run of the existing harness suite with default
     config shows no behavior change; a config-off test confirms a thrashing
     stream runs to the normal cap-reached outcome when detection is disabled.

6. **FR-6: Precedence.** Within one `observe` call, repeated-failure (FR-1) is
   evaluated before thrash (FR-2): an identical *failing* call that is also
   *repeating* is the more specific, operator-recoverable signal and wins. The
   hang watchdog (FR-3) is orthogonal — it fires mid-iteration, before any
   `observe` call for that iteration runs.
   - Acceptance: a stream of identical failing calls that satisfies both
     thresholds lands in `interrupted` (blocked), not `failed_validation`.

7. **FR-7: Audit-stream visibility.** All three new events round-trip verbatim
   through `flywheel.audit.stream(run_id)` / `python -m flywheel.audit` under
   the run's monotonic sequence, alongside the existing
   `harness.iteration_completed` telemetry.
   - Acceptance: an audit-stream assertion confirms each new event kind and its
     payload fields persist and read back.

### Non-Functional Requirements

- **Purity**: `flywheel.loop_guard` imports no `json`/`pathlib`/`io`/`asyncio`,
  no `time`, no `open()` — enforced by a new purity test. The hang watchdog's
  timing lives in `harness.py`. `flywheel.task`, `flywheel.lifecycle`, and
  `flywheel.events` are untouched (no new domain event), so their purity
  invariants hold.
- **Performance**: detector work is O(window) per iteration over already-in-memory
  tuples; the watchdog is one coroutine per iteration with a coarse sleep tick.
  No measurable overhead when detectors are disabled (early return).
- **Safety of defaults**: deterministic detectors key on *identical input
  digests*, never on semantic guesses — a false positive requires the agent to
  literally re-issue a byte-identical tool call, so thrash/blocked defaults can
  ship on without aborting legitimately progressing work.
- **Compatibility**: purely additive `HarnessConfig` fields and new event kinds;
  existing consumers reading current event kinds are unaffected.

## Behavior Specification

### Happy Path

1. The harness builds a `LoopGuard` from `config.loop_guard` at the top of
   `_run_attempt_body`, before `_drive_iterations`.
2. Each iteration runs as today; on return, the harness feeds
   `iteration_result.signals.tool_interactions` to `guard.observe(...)`.
3. A clean run produces no verdict and a watchdog that never fires; the loop
   reaches a terminal envelope or the iteration cap exactly as today.

### Hang watchdog mechanism

- `_drive_iterations` wraps each `invoker(request)` call: it launches the
  invocation as an `asyncio.Task` and a watchdog coroutine that loops on a short
  sleep, comparing `mclock()` to a `last_activity` slot.
- The existing `_on_message` closure (`src/flywheel/harness.py:1866`) updates
  `last_activity = mclock()` on every message — reusing the seam that already
  persists each message, so no second subscription is needed.
- If `mclock() - last_activity > hang_timeout_seconds`, the watchdog sets a
  `hang_tripped` flag and cancels the invocation task. The harness observes the
  cancellation, and because `hang_tripped` is set, routes to the FR-3 path
  rather than `_handle_interrupt`. On normal completion the watchdog is
  cancelled cleanly.

### Error Handling

| Error Condition | Expected Behavior |
| --- | --- |
| A new audit event raises during store write | Same as today — the per-message strict-audit path (`_AuditWriteError`) / `harness.audit_write_failed`; a telemetry write failure does not silently drop a safety transition. |
| Watchdog cancel races a near-simultaneous normal completion | The invocation result, if already produced, is honored; the watchdog flag is checked only when the await actually raised `CancelledError`. No double-finalize. |
| `LoopGuard.observe` raises (e.g. malformed input) | Treated as a harness bug, not swallowed — it propagates to the `_run_attempt` boundary and is recorded via the existing crash path. Detectors must not fail open silently. |

### Edge Cases

| Case | Expected Behavior |
| --- | --- |
| Two distinct tools failing alternately (A,B,A,B) | No consecutive identical-tuple run — repeated-failure does **not** trip (consecutive on the same digest). |
| Identical tuple repeats but spread wider than `thrash_window` | Thrash does **not** trip; the window is the backstop against flagging slow legitimate repetition. |
| Iteration produced zero tool interactions | `observe` is a no-op; counters unchanged. |
| Hang threshold set but a steady `RateLimitEvent` stream arrives | Each event resets the timer; the watchdog does not fire (rate-limit is liveness, per Out of Scope). |
| `max_iterations_per_attempt == 1` | Detectors still evaluate the single iteration's intra-iteration tuple sequence (one agent turn can issue many tool calls); a within-turn repeat can trip. |
| Repeated-failure trips on the final allowed iteration | Routes to `interrupted` (blocked) regardless of cap — the block signal preempts the cap-reached agent-error outcome. |

## Technical Context

### Affected Apps

- `flywheel` (root package): new `loop_guard.py`; `harness.py` wiring +
  watchdog + config fields + three event emits; doc/docstring touch-ups.

### Integration Points

- **Invoker signals** (`src/flywheel/invoker.py:50-108`): `ToolInteraction` /
  `tool_interactions` are the detector input; `on_message`
  (`:191-199`) is the watchdog heartbeat. No invoker change required.
- **Lifecycle state machine** (`src/flywheel/lifecycle.py:31-54`): all three
  target edges already exist — `running -> interrupted`,
  `running -> validating`, `running -> internal_error`. No state-machine change.
- **Retry arm** (`src/flywheel/harness.py:1297-1353`): thrash reuses
  `FAILED_VALIDATION` -> `is_retry_eligible` -> `ready`/`failed`; no new policy.
- **Recoverable-blocked path** (`00004`,
  `src/flywheel/harness.py:1649-1700`): repeated-failure reuses the `Blocked`
  domain event + `interrupted` state + operator `-> ready` recovery that clears
  `blocked_requires_json`.
- **Audit stream** (`flywheel.audit`): new event kinds surface automatically
  once persisted via `_emit` -> `append_event`.

### Relevant Existing Code

- `src/flywheel/harness.py:1816-1939` — `_drive_iterations`, the iteration loop
  and `_on_message` seam to wire into.
- `src/flywheel/harness.py:1515-1535` — cap-reached agent-error path
  (`running -> validating -> failed_validation`); thrash's template.
- `src/flywheel/harness.py:1541-1574` — crash path
  (`running -> internal_error`); hang's template.
- `src/flywheel/harness.py:1602-1700` — explicit-blocked path; repeated-failure's
  template.
- `src/flywheel/harness.py:1389-1471` — `_run_attempt`, where `CancelledError`
  is caught and routed to `_handle_interrupt`; the FR-4 disambiguation point.
- `src/flywheel/harness.py:219-278` — `HarnessConfig`, to extend.
- `src/flywheel/harness.py:114-122` — `_DEFERRED_LOOP_SUBSYSTEMS`, to prune.
- `tests/test_lifecycle_module_purity.py` — the purity-test template to mirror.

## Decisions Log

| Decision | Choice | Rationale |
| --- | --- | --- |
| Detector input | `tool_interactions` tuples between iterations, not mid-stream hooks | The harness already has the full ordered tuple per iteration; no SDK hook subscription needed for thrash/blocked. Keeps those detectors pure. |
| Thrash signal | Identical `(tool, sha256(input))` repetition only | Sub-problem (a) is deterministic; net-diff and novelty need snapshots/metrics that `docs/loop.md:39` calls research. Identical-input repeats are unambiguous no-progress regardless of success. |
| Hang location | Watchdog in `harness.py`, analysis in pure `loop_guard.py` | Timing/asyncio is impure; keeping it out of `loop_guard` preserves a purity test on the detection logic. |
| Hang default | `None` (disabled) | No grounded threshold exists (`docs/loop.md:40`); ship the mechanism, not a guessed number — consistent with `00009`. |
| Thrash/blocked defaults | Conservative, on | They key on byte-identical input digests, so a false positive requires the agent to literally repeat itself — safe to default on, unlike a semantic heuristic. |
| Repeated-failure target state | `interrupted` (blocked), retry budget preserved | Mirrors explicit `blocked`; it is operator-recoverable, and the agent is stuck on an external blocker, not failing the task. |
| Thrash target state | `failed_validation` -> retry policy | Task failure per `docs/loop.md:143`, but routed through the existing retry budget rather than straight-to-`failed`: a fresh attempt's new agent context can break the loop, and a heuristic should not terminate a run without the operator's `max_retries` backstop. |
| Hang target state | `internal_error` | `docs/loop.md:144` classifies hanging as Infrastructure, "indistinguishable from subprocess failure" — same class and state as a crash. |
| New event types | `harness.*` audit events only; no `events.py` domain event | Detections map to existing transitions; the audit event records *why*. Avoids touching `flywheel.events` purity. Implicit-blocked reuses the existing `Blocked` domain event. |
| Precedence | Repeated-failure before thrash | A repeating *failing* call is the more specific, operator-actionable (recoverable) signal. |

## Open Questions

None — design decisions resolved during discovery. Default threshold *values*
(e.g. `repeated_tool_failure_threshold`, `thrash_window`,
`thrash_repeat_threshold`) are tuning knobs to settle during `/task`
implementation; the hang threshold deliberately ships disabled.

## Next Steps

Run `/task 00015-FEATURE-loop-safety-net` to generate implementation tasks from
this spec.
