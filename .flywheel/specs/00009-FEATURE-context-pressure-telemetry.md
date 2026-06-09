# Feature: Context-pressure telemetry

## Summary

Persist the per-iteration token and cost signals that already flow through the
loop but are currently dropped. Each `harness.iteration_completed` event gains a
full token breakdown (input / output / cache-creation / cache-read / total),
plus the SDK-reported cost and turn counts, so the audit stream finally carries
the "Context-aware" signal `docs/vision.md` declares a core principle. MVP is
observability-only and emits no derived utilization% — only SDK-grounded raw
numbers.

## Background

`docs/vision.md:54-65` lists context pressure as a first-class concern (window
utilization over time, growth rate, 50/75/90 threshold crossings, cumulative
token growth). A grep across `src/flywheel/` for any of this returns zero hits —
it is wholly unbuilt.

The data already reaches the harness and is then discarded:

- `InvocationSignals` carries `num_turns`, `total_cost_usd`, and
  `rate_limit_events` (`src/flywheel/invoker.py:99-107`).
- `_build_observation` already sums per-iteration tokens into
  `observation.total_tokens` via `total_tokens_from_usage`
  (`src/flywheel/harness.py:542-556`), used only for transcript-grader breach
  checks and the `harness.budget_exceeded` payload.
- `harness.iteration_completed` persists only
  `{iteration, envelope, failure, stop_reason, rate_limited:bool}`
  (`src/flywheel/harness.py:1647-1663`) — every quantitative value is thrown
  away.

The fix is wiring + persistence through the existing observability seam
(`_emit` -> `store.append_event`, surfaced by the audit stream). It does not
touch the domain-event taxonomy in `src/flywheel/events.py`, so the
`flywheel.task` / `flywheel.lifecycle` / `flywheel.events` purity invariants are
untouched.

## Scope

### In Scope

- Enrich the `harness.iteration_completed` event payload with a per-iteration
  `usage` object: `input_tokens`, `output_tokens`, `cache_creation_input_tokens`,
  `cache_read_input_tokens`, and the summed `total_tokens`.
- Add `total_cost_usd` and `num_turns` to the same payload, sourced from the
  iteration's `InvocationSignals` (the SDK reports these as session-cumulative;
  see Edge Cases).
- Token values are per-iteration deltas. Run-level cumulative totals are derived
  by consumers summing the stream — the harness keeps no running counter.
- Source the breakdown from the iteration's SDK messages (`msg.usage`), reusing
  `total_tokens_from_usage` for the `total_tokens` sum so the new payload and the
  transcript-grader budget math stay consistent.
- Tests covering: payload shape on a normal iteration, an iteration with no
  usage data (all fields `0` / `None`, see Edge Cases), and cost/turns
  passthrough.
- A short doc touch-up in `docs/vision.md` (or the harness module docstring)
  noting that the raw signals are now emitted and that utilization% / thresholds
  remain future work.

### Out of Scope

- **Utilization% and 50/75/90 threshold crossings.** Deferred — they need a
  context-window-capacity source the SDK does not provide. A follow-up feature
  adds capacity + util% + a distinct one-shot threshold-crossing event.
- **Any automated action on pressure** — compaction, reset, halt, or retry
  decisions. Signals are risk indicators, not triggers (`docs/vision.md:65`).
- **A new domain or telemetry event kind.** Signals ride the existing
  `harness.iteration_completed` event; no new event type, no `events.py` change.
- **Stored cumulative state** on the lifecycle or in a running harness counter.
- **Growth-rate derivation.** Computable downstream from per-iteration deltas;
  not emitted.

## Requirements

### Functional Requirements

1. **FR-1: Per-iteration token breakdown.** `harness.iteration_completed`
   payload includes a `usage` object with `input_tokens`, `output_tokens`,
   `cache_creation_input_tokens`, `cache_read_input_tokens`, and `total_tokens`.
   - Acceptance: a harness test driving an iteration whose SDK messages carry
     known `usage` dicts asserts each field and the summed `total_tokens` appear
     in the emitted event's payload.

2. **FR-2: Cost and turn passthrough.** The same payload includes
   `total_cost_usd` and `num_turns` from the iteration's `InvocationSignals`.
   - Acceptance: a test with a `ResultMessage` carrying `total_cost_usd` and
     `num_turns` asserts both surface in the payload; absent values surface as
     `null`.

3. **FR-3: Total consistency.** The payload's `total_tokens` equals
   `total_tokens_from_usage` over the iteration's usage — the same figure the
   transcript-grader breach path uses.
   - Acceptance: a test asserts the emitted `total_tokens` equals
     `observation.total_tokens` for the same iteration.

4. **FR-4: Per-iteration semantics, no harness counter.** Token fields reflect
   only the current iteration; the harness adds no cumulative state.
   - Acceptance: a two-iteration test asserts each event carries that
     iteration's own usage (not a running sum), and that summing the two yields
     the run total.

5. **FR-5: Audit-stream visibility.** The enriched payload is readable verbatim
   through `flywheel.audit.stream(run_id)` / `python -m flywheel.audit`.
   - Acceptance: an audit-stream test (or assertion against the persisted
     `events` row) confirms the new fields round-trip through the store.

### Non-Functional Requirements

- **Performance**: No measurable overhead — the values are already computed
  (`_build_observation`) or already on `InvocationSignals`; this is payload
  assembly only.
- **Security**: Token counts and cost are non-sensitive numerics; no new
  redaction surface. Consistent with the sensitive-by-default audit store.
- **Compatibility**: Purely additive to an existing payload. Existing consumers
  that read `iteration`, `envelope`, `failure`, `stop_reason`, `rate_limited`
  are unaffected.

## Behavior Specification

### Happy Path

1. The harness runs one iteration via `invoke_iteration`.
2. It builds the observation (already summing per-iteration tokens) and reads
   `iteration_result.signals` for cost / turns.
3. When emitting `harness.iteration_completed`, it attaches the `usage` object,
   `total_cost_usd`, and `num_turns` alongside the existing fields.
4. The event lands in the `events` table and is visible in the audit stream;
   a consumer summing `usage.total_tokens` across the run's iteration events
   gets cumulative token growth.

### Error Handling

| Error Condition | Expected Behavior |
| --------------- | ----------------- |
| Emitting the enriched payload raises during store write | Same as today — `harness.audit_write_failed` path; the run is not aborted by a telemetry failure. |
| `InvocationSignals` lacks cost/turns (e.g. failed iteration before `ResultMessage`) | Emit `total_cost_usd: null`, `num_turns: null`; token fields reflect whatever partial usage was observed. |

### Edge Cases

| Case | Expected Behavior |
| ---- | ----------------- |
| Iteration produced no `usage` data at all | All `usage` fields `0`, `total_tokens` `0`; cost/turns `null`. Event still emitted. |
| SDK reports `total_cost_usd` / `num_turns` as session-cumulative (not per-iteration) | Emit verbatim and document the semantic in the payload's field meaning — these two are SDK-cumulative while token fields are per-iteration. Do not attempt to delta them. |
| Resumed session re-sends prior context as `cache_read_input_tokens` | Recorded as-is in the breakdown — the cache-read field is exactly the signal that makes resume cost visible; no de-duplication. |
| Failed iteration (`iteration_result.failure` set) | Usage reflects pre-failure observation; event still carries the breakdown so a crash's token cost is auditable. |

## Technical Context

### Affected Apps

- `flywheel` (root package): harness payload assembly + tests.

### Integration Points

- **Audit stream** (`flywheel.audit` / `python -m flywheel.audit`): the new
  fields surface automatically once persisted via `_emit` -> `append_event`.
- **Transcript grader**: shares `total_tokens_from_usage`; no behavior change,
  but the spec requires the emitted total to match its math (FR-3).

### Relevant Existing Code

- `src/flywheel/harness.py:1643-1666` — the `harness.iteration_completed` emit
  site to enrich.
- `src/flywheel/harness.py:542-556` — `_build_observation`, where per-iteration
  `total_tokens` is already computed.
- `src/flywheel/invoker.py:99-107` — `InvocationSignals` (`num_turns`,
  `total_cost_usd`).
- `src/flywheel/grader_transcript.py:60-83` — `total_tokens_from_usage` and the
  documented usage-key set to reuse.
- `src/flywheel/harness.py:1480-1487` — existing `budget_exceeded` payload, the
  precedent for emitting `{turns, total_tokens, wall_seconds}` numerics.

## Decisions Log

| Decision | Choice | Rationale |
| -------- | ------ | --------- |
| Signal set | Raw counts + cost (no util%) | All values are SDK-grounded; no guessed numbers. |
| Window-capacity source | Defer util% / thresholds entirely | SDK does not report window size; no clean source yet. Avoids baking in numbers that drift. |
| Emission shape | Enrich `harness.iteration_completed` | Numbers describe the same iteration the event already represents; one event per iteration keeps the per-run audit sequence clean and volume flat. A separate kind earns its place only for the deferred threshold-crossing case. |
| Token granularity | Full breakdown (4 fields + total) | Cache-read dominates resumed sessions; the breakdown is where the real pressure signal lives. |
| Accumulation | Per-iteration deltas; consumers sum | Audit stream is append-only and `Lifecycle` is a fold of events — derived totals belong at query time; avoids drifting harness state and cache-read double-counting across attempts. |
| Action scope | Observability only | Vision treats these as risk indicators, not triggers; intervention is a future feature built on this telemetry. |

## Open Questions

None — all design decisions resolved during discovery.

## Next Steps

Run `/task 00009-FEATURE-context-pressure-telemetry` to generate implementation
tasks from this spec.
