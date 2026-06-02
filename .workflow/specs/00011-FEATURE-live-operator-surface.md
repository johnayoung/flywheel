# Feature: Live operator surface

## Summary

Turn `flywheel.workflow live` and the worker `Heartbeat` into a readable,
real-time operator view: per in-flight run, render the lifecycle position
(`ready > RUNNING > validating > grading > done`), the current iteration, the
latest agent action (tool call or text), and the running token / cost / turn
totals. This consumes the message-granular stream that feature 00010 makes live
and the per-iteration token/cost breakdown that feature 00009 already emits —
no new data is produced here, only rendering. MVP enriches the existing CLI
surfaces; it does not add a new dashboard, TUI, or dependency.

## Background

After feature 00010, the durable audit stream is written at message granularity,
so the data needed for a live view is present in the store the instant it
happens. But the current operator surfaces under-render it:

- `collect_live_rows` (`src/flywheel/workflow.py:1311`) returns the *latest*
  single SDK message or event per run; the `live` command and worker
  `Heartbeat` print a terse one-liner (`task status iter=N age=Ns KIND detail`).
- The worker `Heartbeat` (`.workflow/worker.py:502 _format_heartbeat`) reuses
  that same row and prints the same shape.
- The quantitative signals shipped in 00009 — the `harness.iteration_completed`
  payload's `usage` breakdown, `total_cost_usd`, and `num_turns` — are persisted
  but never surfaced in any live view.

The operator question — "what is the agent doing, where are we, how much has it
burned?" — is answerable from data already in the store; it is purely a
rendering gap.

## Scope

### In Scope

- Extend the live row built by `collect_live_rows` to carry: lifecycle status,
  current iteration, the latest SDK message rendered as a short action line
  (tool name + a compact arg preview, or a text snippet), age, and run totals
  summed across the run's `harness.iteration_completed` events
  (`usage.total_tokens`, `total_cost_usd`, `num_turns`).
- A multi-line readable renderer for `flywheel.workflow live` showing the
  lifecycle-position breadcrumb with the current state emphasized, plus the
  action line and the running totals.
- Mirror the same enriched one-liner in the worker `Heartbeat` so the daemon
  operator sees identical signal without a second terminal.
- Keep `--watch` behavior (refresh + clear) and the existing one-shot mode.
- `--json` / machine-readable output stays a faithful superset (additive
  fields), so programmatic consumers are unaffected.
- Tests asserting the rendered fields over a seeded in-progress run.

### Out of Scope

- **A new dashboard / TUI / dependency.** This enriches the existing CLI only.
- **Changing what is persisted.** Token/cost come from 00009; live messages from
  00010. No new event kind, no store-write changes.
- **`flywheel.audit --follow`** stays the raw, unsummarized firehose; `live` is
  the human summary. No change to the audit module's output.
- **Cross-run aggregation / historical charts.** Per-run live view only.
- **Interactivity.** No controls in the view (later features).

## Requirements

### Functional Requirements

1. **FR-1: Lifecycle-position breadcrumb.** Each live run renders the ordered
   lifecycle path with the current state emphasized.
   - Acceptance: a test seeds a run in `validating` and asserts the rendered
     breadcrumb marks `validating` as current.

2. **FR-2: Current action line.** The latest SDK message renders as a compact
   action: tool calls show the tool name and a short arg preview; assistant text
   shows a snippet; result/other types show a labeled summary.
   - Acceptance: a test with a tool-use message asserts the tool name appears;
     a test with a text message asserts the snippet appears.

3. **FR-3: Running totals.** The view shows cumulative `total_tokens`, cost, and
   turns summed across the run's `harness.iteration_completed` events.
   - Acceptance: a two-iteration run asserts the displayed totals equal the sum
     of the two iterations' payloads (consistent with 00009's per-iteration
     semantics).

4. **FR-4: Heartbeat parity.** The worker `Heartbeat` line carries the same
   enriched signal (iteration, action, totals).
   - Acceptance: a test asserts `_format_heartbeat` output includes the action
     and totals for a seeded row.

5. **FR-5: Machine output is an additive superset.** `--json` (or equivalent)
   includes the new fields without removing or renaming existing ones.
   - Acceptance: a test asserts the prior keys are present and unchanged
     alongside the new ones.

### Non-Functional Requirements

- **Performance**: the totals sum is over a run's iteration-completed events
  (one per iteration, few per run); negligible. `--watch` refresh cadence is
  unchanged.
- **Compatibility**: existing `live` / heartbeat consumers that parse the old
  one-liner should be considered; if the human format changes shape, the
  machine-readable path is the stable contract and must stay additive.
- **No new dependency**: rendering uses stdlib only, consistent with the
  existing CLI.

## Behavior Specification

### Happy Path

1. An operator runs `flywheel.workflow live --watch 2` (or watches the worker).
2. Each tick, `collect_live_rows` reads the latest message/event per in-flight
   run and sums the run's iteration-completed totals.
3. The renderer prints, per run: the breadcrumb with the current state, the
   current iteration and age, the latest action line, and `tokens=.. cost=$..
   turns=..`.
4. As 00010 lands messages live, the action line and totals update within a tick
   of the agent acting.

### Error Handling

| Error Condition | Expected Behavior |
| --------------- | ----------------- |
| A run has no iteration-completed events yet | Totals render as zero / `--`; the breadcrumb and action line still render. |
| Latest message lacks a renderable action (unknown type) | Fall back to the message-type label; never crash the view. |
| Totals payload missing a field (older event) | Treat the missing field as zero / `null`; render the rest. |

### Edge Cases

| Case | Expected Behavior |
| ---- | ----------------- |
| Multiple concurrent in-flight runs | One block/line per run, stable ordering (e.g. by task id). |
| Run just transitioned to a terminal state mid-tick | Render its final state in the breadcrumb; drop from in-flight on the next tick per existing `collect_live_rows` filtering. |
| Very long tool args | Truncate the preview to a fixed width; never wrap unboundedly. |

## Technical Context

### Affected Apps

- `flywheel` (root package): `workflow.py` rendering + `collect_live_rows`;
  `.workflow/worker.py` heartbeat formatting; tests.

### Integration Points

- **Feature 00010** (prerequisite): supplies live, message-granular
  `sdk_messages` rows that make the action line update in real time.
- **Feature 00009** (already shipped): supplies the `usage` / `total_cost_usd`
  / `num_turns` fields on `harness.iteration_completed` that feed the totals.
- **`flywheel.audit`**: unchanged; remains the raw stream.

### Relevant Existing Code

- `src/flywheel/workflow.py:1268` — the `live` command.
- `src/flywheel/workflow.py:1311` — `collect_live_rows` and the `LiveRunRow`
  shape to extend.
- `.workflow/worker.py:502` — `_format_heartbeat`; `:515` — the `Heartbeat`
  thread.
- The `harness.iteration_completed` payload (post-00009) — source of the totals.

## Decisions Log

| Decision | Choice | Rationale |
| -------- | ------ | --------- |
| Surface | Enrich existing CLI (`live` + heartbeat) | User chose this over a new TUI; reuses what exists, no new dependency. |
| Totals source | Sum `harness.iteration_completed` events at query time | Matches 00009's per-iteration-delta semantics; no harness counter, no drift. |
| `audit --follow` | Leave as the raw firehose | `live` is the human summary; the two surfaces serve different needs. |
| Machine output | Additive superset | Keeps programmatic consumers stable while the human view evolves. |

## Open Questions

None — design resolved during the observability/interactivity planning pass
(`~/.claude/plans/ok-it-worked-but-spicy-firefly.md`).

## Next Steps

Run `/task 00011-FEATURE-live-operator-surface` after 00010 lands. Feature 00012
(graceful interrupt) is independent of this and can proceed in parallel.
