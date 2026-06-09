# Feature: Manual Grader Execution (Human-Approval Gate)

## Summary

Wire the dormant `manual` grader type into the harness as a human-approval gate — the last unimplemented grader. When command, transcript, and rubric graders all pass, a task that declares `ManualGrader` instances pauses in a new `AWAITING_APPROVAL` lifecycle state instead of promoting straight to `DONE`. An operator approves or rejects each gate out-of-band via two new control verbs (`flywheel approve` / `flywheel reject`); approval of the last gate promotes to `DONE`, rejection routes to `FAILED_VALIDATION` and carries the operator's feedback into the next attempt's prompt — reusing the blocked-lifecycle park/resume machinery (spec 00004), the `control_commands` channel (spec 00013), and the reviewer-feedback retry path (spec 00005).

## Background

`ManualGrader` (`src/flywheel/task.py:46`) is a first-class variant of the `Grader` union, validated by `flywheel.task` and parsed by `flywheel.loaders` (`src/flywheel/loaders.py:108-110`). `docs/task-schema.md:46` documents the cost-ordered chain `command -> transcript -> rubric -> manual`, and `docs/task-schema.md:92` documents the `manual` grader shape. But the harness validation path (`src/flywheel/harness.py:2440-2540`) runs only command, transcript, and rubric graders. `ManualGrader` is never imported into `harness.py` (`harness.py:123` imports `CommandGrader, RubricGrader, Task, TranscriptGrader`).

The consequence is a soundness hole against the loop's core promise. `docs/vision.md` states that completion claims are tested via graders, not trusted, and the harness owns the `DONE` transition. Today a task author can declare a manual approval gate (e.g. "a human must confirm this irreversible migration") and the harness silently ignores it: command/transcript/rubric pass, and the lifecycle is promoted to `DONE` with no human ever consulted. The schema lets authors express a guarantee the loop does not honor. `docs/loop.md:35` flags this as the open TODO in the grader taxonomy ("the `manual` approval surface ... requir[es] separate research phases"); spec 00005 explicitly deferred it (`.workflow/specs/00005...:255`).

This feature is high-leverage because the mechanics it needs already exist and are proven:

- **Park/resume of a non-running lifecycle**: spec 00004 introduced `blocked_requires_json` and `recheck_blocked_lifecycle` (`harness.py:1011+`), which park an `INTERRUPTED` lifecycle, persist a resume condition, and resolve it out-of-band on the orchestrator's reactive sweep. A manual gate is the same shape: park, persist what is pending, resolve later.
- **Operator decision channel**: spec 00013 added the `control_commands` table (`_schema/persistence-schema.sql:207`), recognized verbs (`invoker_client.py:57-61`), and CLI producers (`workflow.py:1938+`, e.g. `_cmd_interrupt`, `_cmd_steer`). `approve` / `reject` are two more verbs on that channel.
- **Reviewer-feedback retry**: spec 00005 added `RubricFindings` and the `# Reviewer feedback` prompt section (`prompt.py`, collected by `_collect_prior_rubric_findings` at `harness.py:928`). A rejection's feedback flows into the next attempt the same way a failing rubric's summary does.

The new piece is a lifecycle state — `AWAITING_APPROVAL` — and a small `grader_manual.py` module plus a resolver. The CLAUDE.md purity invariants are preserved: `flywheel.task`, `flywheel.lifecycle`, and `flywheel.prompt` stay free of `json`/`pathlib`/`io`; the new gate-selection logic is pure; persistence and control-command claiming live in the store and harness/orchestrator layers where IO already lives.

## Scope

### In Scope

- **New lifecycle state `AWAITING_APPROVAL`** on `Status` (`src/flywheel/lifecycle.py:7`), with new valid edges `VALIDATING -> AWAITING_APPROVAL`, `AWAITING_APPROVAL -> DONE`, and `AWAITING_APPROVAL -> FAILED_VALIDATION` in `_VALID_EDGES` (`lifecycle.py:31`). `AWAITING_APPROVAL` is a parked, resumable, non-terminal state that does **not** consume retry budget on entry.
- **New module `src/flywheel/grader_manual.py`** holding pure helpers: `next_pending_manual_gate(task, *, after_ordinal)` (selects the next `ManualGrader` in `task.graders` order strictly after a given ordinal), and `build_manual_result(...)` (assembles a `GraderResultRecord` for an approve/reject decision). No SDK, no subprocess; the module appends results via the supplied `store` exactly as `grader_command.py` does.
- **Harness wiring at the validation seat** (`harness.py:2462-2540`): after command + transcript + rubric all pass, if `task` declares any `ManualGrader`, finalize the attempt `SUCCEEDED` and transition `VALIDATING -> AWAITING_APPROVAL` parked on the first manual gate instead of `VALIDATING -> DONE`. Persist the awaiting gate's ordinal. Emit `harness.awaiting_approval`. (The attempt is finalized at entry — not held open across the human wait; see the `SUCCEEDED` semantics in the NFRs.)
- **New nullable lifecycle column `awaiting_manual_ordinal`** (`_schema/persistence-schema.sql` lifecycles table, plus the Postgres mirror), persisted/loaded by `SqliteStore`, `PostgresStore`, and `MemoryStore`. Cleared centrally on every `-> READY`, `-> DONE`, and `-> FAILED_VALIDATION` edge (mirroring how `blocked_requires_json` clears on `-> READY` in `Lifecycle.transition_to`). Bumps `CURRENT_SCHEMA_VERSION` 4 -> 5 and the `schema_version` CHECK.
- **Two new control verbs `approve` and `reject`** added to the recognized set (`invoker_client.py:57-61` neighborhood). `reject` carries an optional `{"feedback": "..."}` payload. Payload validators mirror `_payload_text` / `_payload_model`.
- **Two new CLI producers**: `flywheel approve RUN_ID` and `flywheel reject RUN_ID [--feedback TEXT]` in `workflow.py`, built on `_enqueue_control_command` (`workflow.py:1938`). The in-flight check accepts `AWAITING_APPROVAL` as the valid target status (the existing producer warns when a run is not in `RUNNING`/`VALIDATING`; for these verbs the valid status is `AWAITING_APPROVAL`).
- **Out-of-band resolver `resolve_manual_approval(lifecycle, store, task, *, now=...)`** in `harness.py`, a sibling of `recheck_blocked_lifecycle`. It claims the oldest pending `approve`/`reject` command for an `AWAITING_APPROVAL` lifecycle and applies it:
  - **approve**: write a `passed=True` manual `GraderResultRecord` for the awaiting gate (keyed to the attempt already finalized `SUCCEEDED` at gate entry); select the next manual gate after it; if one exists, re-park on that gate (update `awaiting_manual_ordinal`, stay `AWAITING_APPROVAL`, emit a fresh `harness.awaiting_approval`); if none remain, transition `AWAITING_APPROVAL -> DONE` (no second attempt-finalize). Emit `harness.manual_approved`.
  - **reject**: write a `passed=False` manual `GraderResultRecord` carrying the operator feedback as its summary; transition `AWAITING_APPROVAL -> FAILED_VALIDATION` (the attempt keeps its `SUCCEEDED` outcome — the agent passed automation; the rejection is recorded by the receipt); the existing `is_retry_eligible` arm then drives `FAILED_VALIDATION -> READY` (retry, consuming budget) or `-> FAILED` (exhausted). Emit `harness.manual_rejected`.
- **Reactive resolution sweep**: the orchestrator's existing reactive unblock pass (which calls `recheck_blocked_lifecycle` on `INTERRUPTED` lifecycles, per `orchestrator.py`) also calls `resolve_manual_approval` on `AWAITING_APPROVAL` lifecycles, so a detached worker daemon applies operator decisions on its next tick.
- **Rejection feedback into retry**: the rejection's feedback surfaces in the next attempt's prompt. The collection helper at `harness.py:928` (`_collect_prior_rubric_findings`) is generalized to also collect failing `manual` grader receipts; the prompt's reviewer-feedback section (spec 00005) renders manual rejections labeled distinctly from rubric findings.
- **`finalize_stranded_lifecycle` exemption**: the SIGKILL/OOM/reboot backstop (`docs/task-lifecycle.md:41`) must treat `AWAITING_APPROVAL` as a legitimate parked state, not a stranded `RUNNING`/`VALIDATING` to finalize.
- **Surfacing**: `flywheel status` and `flywheel live` render `AWAITING_APPROVAL` and the pending instruction(s) so an operator knows a decision is owed. `harness.awaiting_approval` carries `{instructions, awaiting_ordinal, grader_name, run_id, attempt_number, artifacts_dir}`.
- **Docs**: update `docs/task-lifecycle.md` (new state row, diagram, edges, rules), `docs/loop.md` (retract the manual-grader TODO at line 35; document the gate and the verbs), `docs/task-schema.md` (note `manual` is now executed and how it pauses), `docs/vision.md` (the manual approval surface is implemented).

### Out of Scope

- **Approval timeout / auto-resolution.** A parked gate waits indefinitely for an operator, mirroring `blocked` semantics (spec 00004 has no auto-timeout). No timer machinery, no `timed_out` outcome. Grounding a default timeout is its own work (compare the deferred hang-timeout default in `_DEFERRED_LOOP_SUBSYSTEMS`).
- **Approving/rejecting via the live in-session steering watcher.** `AWAITING_APPROVAL` is a parked state with no live `ClaudeSDKClient`; `approve`/`reject` are applied only by the out-of-band `resolve_manual_approval`. The live watcher (`invoker_client.py`) treats these verbs as not-applicable (recorded but not dispatched to a session).
- **Combined "approve all gates at once."** Per the answered design, gates are sequential: each `ManualGrader` is approved or rejected independently, in `task.graders` order. One reject short-circuits the remaining gates (consistent with within-type cost-order short-circuit for command/rubric).
- **Per-gate partial reject feedback affecting which gate retries.** A reject fails the whole attempt; the retry re-runs the agent from the top, then re-enters all gates. The loop does not resume mid-gate after a reject.
- **Operator dashboard / web surface for approvals.** Consumer concern; surfaced via `flywheel status` / `flywheel live` / the audit stream only.
- **Changing `CommandGrader`, `TranscriptGrader`, or `RubricGrader` behavior.** Their runners are untouched. The `ManualGrader` schema and loader are already complete (`task.py:46`, `loaders.py:108`) — no change to the dataclass or parser.
- **A `name`-keyed approve (approving a specific gate by name rather than the currently-awaiting one).** v1 approves the gate the lifecycle is currently parked on; sequential ordering makes "which gate" unambiguous.
- **Auto-deriving approval context beyond instruction + pointers.** The `harness.awaiting_approval` event carries instruction text plus `run_id`/`attempt_number`/`artifacts_dir`; the operator inspects what the agent did through the existing `flywheel live` / `python -m flywheel.audit` surfaces. No new rendering of diffs/artifacts in the gate event.

## Requirements

### Functional Requirements

1. **FR-1 — `AWAITING_APPROVAL` lifecycle state.** `Status` gains `AWAITING_APPROVAL = "awaiting_approval"` (`lifecycle.py:7`). `_VALID_EDGES` (`lifecycle.py:31`) adds `VALIDATING -> AWAITING_APPROVAL`, `AWAITING_APPROVAL -> DONE`, and `AWAITING_APPROVAL -> FAILED_VALIDATION`. `AWAITING_APPROVAL` is **not** in `_RETRY_SOURCE_STATUSES` (entering it costs no retry) and **not** in `_REQUIRES_ERROR` (entering it requires no `error`). It is non-terminal.
   - Acceptance: `tests/test_lifecycle.py` covers each new edge is permitted and that all other transitions out of `AWAITING_APPROVAL` raise `LifecycleTransitionError`; that `AWAITING_APPROVAL` does not consume retry budget; that `is_retry_eligible` is unaffected by it. `tests/test_lifecycle_module_purity.py` still passes (no IO added).

2. **FR-2 — `awaiting_manual_ordinal` persistence.** The `lifecycles` table gains a nullable `awaiting_manual_ordinal` column (`_schema/persistence-schema.sql`, Postgres mirror). All three stores (`store_memory`, `store_sqlite`, `store_postgres`) persist and load it. The value is the ordinal (index in `task.graders`) of the manual gate currently awaiting a decision; it is `NULL` in every state except `AWAITING_APPROVAL`. It is cleared on every transition to `READY`, `DONE`, and `FAILED_VALIDATION` (alongside the existing `blocked_requires_json` clearing in `Lifecycle.transition_to`). `CURRENT_SCHEMA_VERSION` becomes 5; the `schema_version` CHECK is updated.
   - Acceptance: store contract tests assert round-trip of `awaiting_manual_ordinal`; assert it is cleared on the three edges; a migration test asserts a v4 database opens and reads `NULL` for the new column. `tests/test_store_sqlite.py` / Postgres equivalents cover the new column.

3. **FR-3 — Pure gate selection (`grader_manual.py`).** `next_pending_manual_gate(task, *, after_ordinal: int | None) -> ManualGate | None` returns the first `ManualGrader` in `task.graders` order whose ordinal is strictly greater than `after_ordinal` (or the first manual gate when `after_ordinal is None`), as a small frozen `ManualGate(ordinal, instruction, grader_name)`. `build_manual_result(gate, *, run_id, attempt_number, passed, summary, now) -> GraderResultRecord` assembles a `grader_type="manual"` receipt. The module imports no `json`/`io`/SDK; it is pure data + record assembly.
   - Acceptance: `tests/test_grader_manual.py` covers: (a) first gate selected when `after_ordinal=None`; (b) next gate selected strictly after a given ordinal; (c) `None` when no manual gate remains; (d) non-manual graders are skipped while preserving each manual grader's `task.graders` ordinal; (e) `build_manual_result` produces the documented payload for approve and reject.

4. **FR-4 — Harness gate entry.** In the validation seat (`harness.py`, after the rubric block at `harness.py:2527` and before the `DONE` transition at `harness.py:2539`): when `command_passed and transcript_passed and rubric_passed` and `task` declares at least one `ManualGrader`, the harness **finalizes the attempt `SUCCEEDED`** (the agent's work passed every automated grader), then — instead of `VALIDATING -> DONE` — selects the first manual gate via `next_pending_manual_gate(task, after_ordinal=None)`, persists `awaiting_manual_ordinal`, transitions `VALIDATING -> AWAITING_APPROVAL`, and emits `harness.awaiting_approval` with `{instructions, awaiting_ordinal, grader_name, run_id, attempt_number, artifacts_dir}`. No attempt remains open while parked: the human decision is a lifecycle-level gate, not part of the attempt's duration (see the `SUCCEEDED` semantics in the NFRs).
   - Acceptance: `tests/test_harness.py` covers: (a) all-automated-pass + one manual gate reaches `AWAITING_APPROVAL` (not `DONE`) with the attempt finalized `SUCCEEDED`, `awaiting_manual_ordinal` set to the gate's ordinal, and `harness.awaiting_approval` emitted; (b) all-automated-pass + zero manual gates still reaches `DONE` (byte-identical to today's path — no new events); (c) a command/transcript/rubric failure never reaches the manual gate.

5. **FR-5 — Approve resolution.** `resolve_manual_approval` applied to an `approve` command on an `AWAITING_APPROVAL` lifecycle: writes a `passed=True` manual `GraderResultRecord` (keyed to the already-finalized attempt) for the gate at `awaiting_manual_ordinal`; selects `next_pending_manual_gate(task, after_ordinal=<current>)`; if a next gate exists, updates `awaiting_manual_ordinal` to it, stays `AWAITING_APPROVAL`, and emits a fresh `harness.awaiting_approval`; if none remain, transitions `AWAITING_APPROVAL -> DONE` (the attempt was finalized `SUCCEEDED` at gate entry — no second finalize). Emits `harness.manual_approved` with `{grader_name, awaiting_ordinal}`. The applied control command is recorded via the existing `harness.control_command_applied` event.
   - Acceptance: `tests/test_harness.py` covers: (a) single-gate approve reaches `DONE` with a `passed=True` manual receipt and `awaiting_manual_ordinal` cleared, and the attempt outcome stays `SUCCEEDED` (set at entry, not re-finalized); (b) multi-gate sequential approve re-parks on each subsequent gate and only the final approve reaches `DONE`; (c) each approve appends exactly one manual `GraderResultRecord`.

6. **FR-6 — Reject resolution with feedback retry.** `resolve_manual_approval` applied to a `reject` command: writes a `passed=False` manual `GraderResultRecord` (keyed to the already-finalized `SUCCEEDED` attempt) whose summary is the command's `feedback` payload (or a `"(no feedback provided)"` placeholder when absent); transitions `AWAITING_APPROVAL -> FAILED_VALIDATION` with `error = "manual grader '<name>' rejected by operator"` (no attempt re-finalize — the attempt's `SUCCEEDED` outcome accurately records that the agent passed every automated grader; the rejection is captured by the manual receipt). The existing retry arm then drives `FAILED_VALIDATION -> READY` (when `is_retry_eligible`, consuming retry budget on that edge) or `-> FAILED` (exhausted, retaining the rejection error and feedback in the receipt). A reject short-circuits any later manual gates. Emits `harness.manual_rejected` with `{grader_name, awaiting_ordinal, feedback}`.
   - Acceptance: `tests/test_harness.py` covers: (a) reject with retries remaining reaches `FAILED_VALIDATION` then `READY`, consuming one retry, while the rejected attempt's outcome stays `SUCCEEDED`; (b) reject with retries exhausted reaches `FAILED` with the rejection error; (c) reject on the first of multiple gates does not evaluate later gates; (d) the manual receipt carries the feedback text.

7. **FR-7 — Rejection feedback in the next prompt.** `_collect_prior_rubric_findings` (`harness.py:928`) is generalized (or paired with a sibling collector) to also gather the prior attempt's failing `manual` grader receipts. The reviewer-feedback section of the iteration prompt (spec 00005, `prompt.py`) renders manual rejections with a label distinguishing them from rubric findings (e.g. `manual <name> (operator): <feedback>`). `flywheel.prompt` stays pure — it receives plain data, not a store reference.
   - Acceptance: `tests/test_prompt.py` covers: (a) a manual rejection renders in the reviewer-feedback section with the operator label; (b) empty findings render no section (byte-identical to today); (c) mixed rubric + manual findings render deterministically in `(attempt_number, ordinal)` order. `tests/test_harness.py` covers end-to-end: a rejected first attempt followed by a second attempt whose prompt contains the operator feedback.

8. **FR-8 — `approve`/`reject` control verbs and producers.** The recognized control-command verbs (`invoker_client.py:57-61` neighborhood) gain `CONTROL_COMMAND_APPROVE = "approve"` and `CONTROL_COMMAND_REJECT = "reject"`. `reject` validates an optional `{"feedback": str}` payload (string or absent; non-string is a payload error mirroring `_payload_model`). The live in-session watcher does not dispatch these verbs to a session (they are out-of-band only) and records them as not-applied rather than crashing. New CLI producers `flywheel approve RUN_ID` and `flywheel reject RUN_ID [--feedback TEXT]` enqueue one row via `_enqueue_control_command` (`workflow.py:1938`); the not-in-flight warning treats `AWAITING_APPROVAL` as the valid target status for these verbs.
   - Acceptance: `tests/test_invoker_client.py` (or equivalent) covers the new verb constants and reject-payload validation. `tests/test_workflow.py` covers: (a) `flywheel approve` enqueues a `kind=approve` row; (b) `flywheel reject --feedback X` enqueues `kind=reject` with the feedback payload; (c) enqueuing against a non-`AWAITING_APPROVAL` run prints the pending/stale note; (d) an unknown `run_id` exits 2.

9. **FR-9 — Reactive resolution and stranded-lifecycle exemption.** The orchestrator's reactive sweep calls `resolve_manual_approval` on `AWAITING_APPROVAL` lifecycles (alongside `recheck_blocked_lifecycle` on `INTERRUPTED` ones). When no `approve`/`reject` command is pending, the lifecycle stays parked. `finalize_stranded_lifecycle` excludes `AWAITING_APPROVAL` from the set of statuses it finalizes on worker restart — it is a durable park with **no open attempt** (the attempt was finalized `SUCCEEDED` at gate entry per FR-4), so the backstop's "an open attempt signals a strand" rule is unaffected; only the parked status needs exempting.
   - Acceptance: `tests/test_orchestrator.py` covers: (a) a pending approve is applied on the next reactive tick and the lifecycle advances; (b) with no pending command the lifecycle remains `AWAITING_APPROVAL`. `tests/test_harness.py` (or the stranded-lifecycle test) covers that `finalize_stranded_lifecycle` leaves an `AWAITING_APPROVAL` lifecycle (and its finalized attempt) untouched.

10. **FR-10 — Operator surfacing.** `flywheel status` and `flywheel live` display `AWAITING_APPROVAL` runs with the pending gate's instruction text so the owed decision is visible. The `harness.awaiting_approval` event carries the instruction(s), the awaiting ordinal, the grader name, the run id, the attempt number, and the artifacts dir.
    - Acceptance: `tests/test_workflow.py` covers that `status`/`live` render the awaiting state and instruction for a parked lifecycle; an audit test asserts the `harness.awaiting_approval` payload shape.

### Non-Functional Requirements

- **Purity invariants preserved.** `flywheel.task`, `flywheel.lifecycle`, and `flywheel.prompt` remain pure (no `json`/`pathlib`/`io`/SDK imports); enforced by `tests/test_task_module_purity.py` and `tests/test_lifecycle_module_purity.py`, which must still pass unmodified. `grader_manual.py` is pure record-assembly + selection; the resolver and producers that do IO live in `harness.py`, `orchestrator.py`, `workflow.py`, and the stores, where IO already lives.
- **`SUCCEEDED` semantics (clarified).** `Outcome.SUCCEEDED` means the agent's attempt passed every *automated* grader (command/transcript/rubric); it is **not** coupled to the lifecycle reaching `DONE`. A lifecycle reaches `DONE` iff its attempt is `SUCCEEDED` *and* every manual gate is approved. Consequently a `SUCCEEDED` attempt may be followed by a retry when a gate is rejected — the manual `GraderResultRecord(passed=False)` is the explanation, sitting in the event stream between `AttemptFinalized(SUCCEEDED)` and `TransitionedTo(FAILED_VALIDATION)`. This decoupling is already permitted by the event reducer, which folds `AttemptFinalized` and `TransitionedTo` independently (`events.py:228-296`); no code enforces `SUCCEEDED ⟺ DONE`. The reasoning: an attempt measures the *agent*, and a human rejection is a judgment call, not an agent validation failure — recording it as the agent's `VALIDATION_FAILED` would misattribute it and pollute agent-quality metrics. `docs/task-lifecycle.md` must state this invariant explicitly.
- **Attempt clock measures the agent, not human wall-clock.** The attempt is finalized at gate entry, so `started_at -> ended_at` never spans the (unbounded, human-paced) approval wait. This is the decisive reason the manual gate is modeled as a post-attempt lifecycle gate rather than an in-attempt grader like rubric: the other three graders run in bounded machine time, manual does not.
- **Append-only grader receipts.** Manual receipts are written once, at resolution time (approve or reject), never updated — consistent with the append-only `grader_results` trigger (`store_sqlite`/`store_postgres`). They are keyed by `attempt_number` and legitimately appended *after* their attempt finalizes (the human evaluation happens after the agent's attempt closes). The `AWAITING_APPROVAL` park itself writes no grader receipt (only the awaiting event + the ordinal column).
- **Concurrency.** Optimistic concurrency on `Lifecycle.version` continues to gate every `AWAITING_APPROVAL -> *` transition. Control commands are claimed via the existing single-claim semantics (`idx_control_commands_pending`), so two workers cannot both apply the same approve.
- **Durability.** `AWAITING_APPROVAL` survives worker restart: the state and the awaiting ordinal are persisted; the attempt is already finalized (`SUCCEEDED`), so there is no open attempt to recover. Resolution reads the parked state back. The backstop must not finalize a parked gate (FR-9).
- **Schema migration.** Bumping `CURRENT_SCHEMA_VERSION` 4 -> 5 adds one nullable column; a v4 database remains readable (the column reads `NULL`). No data backfill needed (no in-flight lifecycle can be `AWAITING_APPROVAL` before this version exists).
- **Security.** No new tool surface, no new external input beyond an operator's feedback string (persisted as a grader receipt summary; subject to the existing read-time redaction layer, spec 00014, when surfaced through `python -m flywheel.audit`).
- **Backward compatibility.** Tasks with no `ManualGrader` are byte-for-byte unaffected: the gate-entry branch is skipped and the lifecycle reaches `DONE` exactly as today (FR-4b).

## Behavior Specification

### Happy Path (single gate, approved)

1. A task declares a manual gate:
   ```json
   { "type": "manual", "instruction": "Confirm the migration is safe to run against production data." }
   ```
2. The agent emits `intent=verify`; command + transcript + rubric graders all pass.
3. The harness finalizes the attempt `SUCCEEDED` (automated graders passed), selects the first manual gate, persists `awaiting_manual_ordinal`, transitions `VALIDATING -> AWAITING_APPROVAL`, and emits `harness.awaiting_approval` with the instruction, `run_id`, `attempt_number`, and `artifacts_dir`.
4. `flywheel status` shows the run `AWAITING_APPROVAL` with the instruction. The operator inspects the work via `flywheel live` / `python -m flywheel.audit`.
5. The operator runs `flywheel approve RUN_ID`, enqueuing one `control_commands` row (`kind=approve`).
6. On the next reactive sweep, `resolve_manual_approval` claims the command, writes a `passed=True` manual `GraderResultRecord`, finds no further gate, transitions `AWAITING_APPROVAL -> DONE` (no second attempt-finalize), and emits `harness.manual_approved` + `harness.control_command_applied`.

### Sequential Gates (multiple manual graders)

1. The task declares two manual gates A (ordinal 3) and B (ordinal 4).
2. Automated graders pass; the harness finalizes the attempt `SUCCEEDED` and parks on A (`awaiting_manual_ordinal=3`).
3. `flywheel approve RUN_ID` -> resolver writes A's `passed=True` receipt, selects B, re-parks (`awaiting_manual_ordinal=4`, stays `AWAITING_APPROVAL`), emits a fresh `harness.awaiting_approval` for B.
4. `flywheel approve RUN_ID` -> resolver writes B's `passed=True` receipt, no further gate, `AWAITING_APPROVAL -> DONE`.

### Reject-with-Feedback Path

1. Automated graders pass; the harness finalizes the attempt `SUCCEEDED` and parks on gate A.
2. The operator runs `flywheel reject RUN_ID --feedback "The migration drops a column still read by the billing service. Gate it behind a feature flag first."`.
3. The resolver writes a `passed=False` manual receipt with that feedback as the summary (keyed to the `SUCCEEDED` attempt — the agent passed automation; the human caught what automation could not), transitions `AWAITING_APPROVAL -> FAILED_VALIDATION` with `error = "manual grader 'confirm-migration' rejected by operator"`, emits `harness.manual_rejected`.
4. `is_retry_eligible(max_retries)` is `True`; the harness drives `FAILED_VALIDATION -> READY` (consuming one retry) and starts a fresh attempt.
5. Before invoking the agent, the harness collects the prior attempt's failing manual receipt and populates the reviewer-feedback findings. `build_iteration_prompt` renders:
   ```markdown
   # Reviewer feedback

   The reviewer flagged the following on attempt #1:

   - manual `confirm-migration` (operator): The migration drops a column still read by the billing service. Gate it behind a feature flag first.
   ```
6. The agent reads the feedback, gates the migration, emits `intent=verify`; automated graders pass; the harness re-parks on the gate; the operator approves; lifecycle reaches `DONE`.

### Error Handling

| Error Condition | Expected Behavior |
| --------------- | ----------------- |
| Operator rejects with retries remaining | `AWAITING_APPROVAL -> FAILED_VALIDATION -> READY`. Feedback surfaces in the next attempt's prompt. Consumes one retry. |
| Operator rejects with retries exhausted | `AWAITING_APPROVAL -> FAILED_VALIDATION -> FAILED`. `error` records the rejected gate name; the feedback remains in the manual `GraderResultRecord`. |
| `flywheel approve`/`reject` against a run that is not `AWAITING_APPROVAL` | Producer enqueues the row but prints the existing not-in-flight stderr note (the command sits pending / stale per claim semantics). It is never applied because the resolver only acts on `AWAITING_APPROVAL` lifecycles. |
| `flywheel approve`/`reject` against an unknown `run_id` | Producer exits 2 with "run is unknown to this store" (existing `_enqueue_control_command` FK behavior). |
| Both an `approve` and a `reject` are pending for the same gate | The resolver claims the oldest by `id` (single-claim semantics via `idx_control_commands_pending`); the other remains pending and is applied only if the lifecycle is still `AWAITING_APPROVAL` on a later tick (e.g. a re-park on the next gate). |
| Worker process dies while a gate is parked | `AWAITING_APPROVAL` and `awaiting_manual_ordinal` are durable; the attempt is already finalized (`SUCCEEDED`), so there is no open attempt to recover. On restart, `finalize_stranded_lifecycle` leaves the park untouched; the reactive sweep resumes resolving pending commands. |
| `reject` payload `feedback` is a non-string | Payload validation error (mirrors `_payload_model`); the producer rejects it before enqueuing. |
| `reject` with no `--feedback` | Allowed; the manual receipt summary is `"(no feedback provided)"`; the reviewer-feedback section still renders the gate name so the agent knows it was rejected. |
| Live in-session watcher receives an `approve`/`reject` row | Not dispatched to the session (no live session in `AWAITING_APPROVAL`); recorded as not-applicable rather than crashing. |

### Edge Cases

| Case | Expected Behavior |
| ---- | ----------------- |
| Task declares manual graders interleaved with command/rubric (e.g. order `[command, manual, rubric]`) | Cost order still governs *type* execution order (`command -> transcript -> rubric -> manual`); manual gates are evaluated last regardless of list position. Within the manual type, gates are evaluated in `task.graders` order. |
| Task declares zero manual graders | Gate-entry branch skipped; `VALIDATING -> DONE` exactly as today. No new events, no column write. |
| Approve enqueued before the lifecycle reaches `AWAITING_APPROVAL` | Row sits pending; applied on the first reactive sweep after the gate is reached (claim semantics). The producer's stderr note warns it is not yet in-flight. |
| Two manual gates, reject on the second after approving the first | First gate's `passed=True` receipt persists; second gate's reject writes `passed=False` and routes `AWAITING_APPROVAL -> FAILED_VALIDATION` (the attempt's `SUCCEEDED` outcome is unchanged). On retry, a fresh attempt runs and both gates are re-evaluated from scratch (no mid-gate resume). |
| `SUCCEEDED` attempt followed by a retry (after a reject) | Expected, not a contradiction. The attempt outcome records that automated verification passed; the manual `GraderResultRecord(passed=False)` and the `TransitionedTo(FAILED_VALIDATION)` event record why the lifecycle did not reach `DONE`. See the `SUCCEEDED` semantics NFR. |
| Operator never responds | Lifecycle stays `AWAITING_APPROVAL` indefinitely; visible in `flywheel status`/`live`. No timeout (out of scope). |
| Rubric `retry_on_fail=False` already parks at `INTERRUPTED`; manual parks at `AWAITING_APPROVAL` | Distinct states with distinct meanings: `INTERRUPTED` = blocked/operator-review for rubric or signal; `AWAITING_APPROVAL` = a declared human gate. Both resumable, neither consumes retry on entry. |
| Feedback summary is empty string | Treated as no feedback: `"(no feedback provided)"` placeholder, matching the rubric empty-summary handling (spec 00005 edge case). |

## Technical Context

### Affected Modules

- `src/flywheel/lifecycle.py` — **add** `Status.AWAITING_APPROVAL`; **add** three edges to `_VALID_EDGES` (`lifecycle.py:31`); ensure it is absent from `_RETRY_SOURCE_STATUSES` (`lifecycle.py:61`) and `_REQUIRES_ERROR` (`lifecycle.py:57`); clear `awaiting_manual_ordinal` on `-> READY`/`-> DONE`/`-> FAILED_VALIDATION` inside `transition_to`. Pure module — no IO.
- `src/flywheel/grader_manual.py` — **NEW**. `ManualGate` frozen dataclass, `next_pending_manual_gate`, `build_manual_result`. Pure.
- `src/flywheel/harness.py` — gate entry at the validation seat (`harness.py:2462-2540`); `resolve_manual_approval` sibling of `recheck_blocked_lifecycle` (`harness.py:1011+`); generalize `_collect_prior_rubric_findings` (`harness.py:928`) to include failing manual receipts; new events `harness.awaiting_approval`, `harness.manual_approved`, `harness.manual_rejected`; exempt `AWAITING_APPROVAL` from `finalize_stranded_lifecycle`. Import `ManualGrader` from `flywheel.task` (currently absent at `harness.py:123`).
- `src/flywheel/prompt.py` — render manual rejections in the reviewer-feedback section (spec 00005). Stays pure; receives plain finding data.
- `src/flywheel/invoker_client.py` — add `CONTROL_COMMAND_APPROVE` / `CONTROL_COMMAND_REJECT` (`invoker_client.py:57-61`); reject-payload validator; the live watcher records-but-does-not-dispatch these verbs.
- `src/flywheel/workflow.py` — `flywheel approve` / `flywheel reject` producers built on `_enqueue_control_command` (`workflow.py:1938`); the not-in-flight check accepts `AWAITING_APPROVAL`; `status`/`live` render the awaiting gate.
- `src/flywheel/orchestrator.py` — reactive sweep calls `resolve_manual_approval` on `AWAITING_APPROVAL` lifecycles (alongside the existing `recheck_blocked_lifecycle` pass).
- `src/flywheel/store_protocols.py`, `store_memory.py`, `store_sqlite.py`, `store_postgres.py` — persist/load `awaiting_manual_ordinal`; bump `CURRENT_SCHEMA_VERSION` to 5.
- `src/flywheel/_schema/persistence-schema.sql` (+ Postgres mirror) — add nullable `awaiting_manual_ordinal` to `lifecycles`; update the `schema_version` CHECK to 5.
- `docs/task-lifecycle.md` — new state row (`lifecycle.md:5-17`), diagram (`lifecycle.md:19-32`), edges and rules; note `AWAITING_APPROVAL` does not consume retry and clears `awaiting_manual_ordinal` on the same edges as `blocked_requires_json`; **state the clarified `SUCCEEDED` invariant** — `Outcome.SUCCEEDED` means all automated graders passed, and a lifecycle reaches `DONE` iff `SUCCEEDED` and all manual gates are approved (so a `SUCCEEDED` attempt may precede a retry when a gate is rejected).
- `docs/loop.md` — retract the manual-grader TODO (`loop.md:35`); document the gate, the `approve`/`reject` verbs, and the parked-resolution model (`loop.md:109`).
- `docs/task-schema.md` — note `manual` is now executed and pauses to `AWAITING_APPROVAL` (`task-schema.md:46`, `task-schema.md:92`).
- `docs/vision.md` — the manual approval surface is implemented (removes it from the open grader-taxonomy gap).

### Integration Points

- **Blocked-lifecycle park/resume (spec 00004)**: `resolve_manual_approval` mirrors `recheck_blocked_lifecycle` (`harness.py:1011+`) — out-of-band resolution of a parked, non-running lifecycle, driven by the orchestrator's reactive sweep. `awaiting_manual_ordinal` mirrors `blocked_requires_json` (persisted resume condition, cleared on `-> READY`).
- **Control-command channel (spec 00013)**: `approve`/`reject` are two more verbs on `control_commands` (`_schema/persistence-schema.sql:207`), produced by new CLI subcommands built on `_enqueue_control_command` (`workflow.py:1938`), recorded via `harness.control_command_applied`. The single-claim index (`idx_control_commands_pending`, `:217`) prevents double-apply.
- **Reviewer-feedback retry (spec 00005)**: rejection feedback flows through the same `_collect_prior_rubric_findings` -> `IterationInputs` -> `# Reviewer feedback` path; the rubric retry arm (`FAILED_VALIDATION -> READY`) is reused verbatim for rejects.
- **Read-time redaction (spec 00014)**: operator feedback persisted in a manual receipt is subject to the default redaction policy when surfaced via `python -m flywheel.audit`.

### Relevant Existing Code

- `src/flywheel/lifecycle.py:7-17` — `Status` enum (gains `AWAITING_APPROVAL`).
- `src/flywheel/lifecycle.py:31-63` — `_VALID_EDGES`, `_REQUIRES_ERROR`, `_RETRY_SOURCE_STATUSES` (the membership sets to update).
- `src/flywheel/harness.py:2440-2540` — the validation seat; gate entry slots in after the rubric block (`:2527`) and before `VALIDATING -> DONE` (`:2539`).
- `src/flywheel/harness.py:1011-1130` — `recheck_blocked_lifecycle` and `blocked_requires_json` round-trip: the structural template for `resolve_manual_approval` and `awaiting_manual_ordinal`.
- `src/flywheel/harness.py:928-975` — `_collect_prior_rubric_findings`: the collector to generalize for manual receipts.
- `src/flywheel/harness.py:123` — the `flywheel.task` import line that must add `ManualGrader`.
- `src/flywheel/grader_command.py` — structural template for `grader_manual.py`'s record assembly.
- `src/flywheel/invoker_client.py:57-61` — control-verb constants (gain `approve`/`reject`); `_payload_text`/`_payload_model` (`:88-109`) — validator templates.
- `src/flywheel/workflow.py:1926-2010` — steering producers (`_enqueue_control_command`, `_cmd_interrupt`, `_cmd_steer`): the template for `_cmd_approve` / `_cmd_reject`.
- `src/flywheel/task.py:46-52` — `ManualGrader` (already complete; no change).
- `src/flywheel/loaders.py:108-110` — manual parsing (already complete; no change).
- `src/flywheel/_schema/persistence-schema.sql:59-69, 207-227` — `lifecycles` columns, `control_commands`, `schema_version` CHECK.
- `docs/task-lifecycle.md:5-42` — states, diagram, and rules to extend.
- `docs/loop.md:35, 109` — the manual-grader TODO to retract and the grader-type note.

## Decisions Log

| Decision | Choice | Rationale |
| -------- | ------ | --------- |
| Paused-state modeling | **New `AWAITING_APPROVAL` status** (not reuse of `INTERRUPTED` + predicate) | A declared human gate is semantically distinct from a signal/operator interrupt or an envelope-blocked pause. A dedicated state makes `flywheel status`/`live`/audit filtering unambiguous and keeps the `blocked_requires_json` predicate machinery focused on auto-evaluable predicates. Cost: touches the `Status` enum, edges, stores, and docs — accepted for the clarity. |
| Resume mechanism | **New `approve`/`reject` control verbs** (not a new blocked-requirement predicate) | A human "yes/no" has no auto-evaluable source, so the predicate route (`recheck-blocked`) would need a side-channel anyway. An explicit operator command on the spec-00013 channel records the decision as a first-class, audited action. |
| Reject outcome | **`FAILED_VALIDATION` + feedback retry** (not terminal `FAILED`) | Treats a human "no" like a failing rubric: route to a retry source, carry the critique into the next attempt (spec 00005 path). Preserves the "fix it and try again" loop; retries exhausted still reach `FAILED`. |
| No-response behavior | **Wait indefinitely (MVP)** | Mirrors `blocked` semantics (spec 00004 has no auto-timeout). Visible in `status`/`live`. Avoids new timer machinery and an ungrounded timeout default (compare the deferred hang-timeout). |
| Multiple gates | **Sequential per-gate** | Each `ManualGrader` is approved/rejected independently in `task.graders` order; one reject short-circuits the rest. Granular and matches within-type cost-order short-circuit. |
| Approval context surfaced | **Instruction + audit/artifact pointers** | `harness.awaiting_approval` carries instruction(s) + `run_id`/`attempt_number`/`artifacts_dir`; operator inspects via existing `flywheel live`/audit. No new rendering work (reuses specs 00009-00011). |
| Decision applier | **Out-of-band `resolve_manual_approval` on the reactive sweep** (not the live in-session watcher) | `AWAITING_APPROVAL` is a parked state with no live `ClaudeSDKClient`; resolution must work against a detached daemon, exactly like `recheck_blocked_lifecycle`. |
| Attempt finalization timing | **Finalize the attempt `SUCCEEDED` on entry to `AWAITING_APPROVAL`; the human decision is a lifecycle-level gate, not the attempt's outcome or duration** | An attempt measures the *agent*; the manual gate is an unbounded human wait, so leaving `ended_at` open across it would poison attempt latency/cost telemetry. A human rejection is a judgment call, not the agent failing validation — recording it as `VALIDATION_FAILED` would misattribute it. The event reducer folds `AttemptFinalized` and `TransitionedTo` independently (`events.py:228-296`) and nothing enforces `SUCCEEDED ⟺ DONE`, so a `SUCCEEDED` attempt under an `AWAITING_APPROVAL`/`FAILED_VALIDATION` lifecycle is representable and accurate. Also keeps every parked state's attempt closed (no stranded-attempt special case). The rejection is captured by the manual `GraderResultRecord(passed=False)` + the transition. |
| Persistence shape | **Single nullable `awaiting_manual_ordinal` column** | Sequential gates mean only one gate awaits at a time; remaining gates are re-derived from `task.graders` at resolve time (the resolver already loads the task, as `recheck` does). One integer column is sufficient and minimal. |
| Schema/loader for `ManualGrader` | **No change** | The dataclass (`task.py:46`) and loader (`loaders.py:108`) already support `manual`; only harness execution was missing. |

## Open Questions

None blocking v1. Items for follow-ups:

- **Approve-by-name.** v1 approves the gate the lifecycle is parked on (unambiguous under sequential ordering). A `--gate NAME` selector could be added if operators want to pre-stage decisions.
- **Bulk/auto approval policy.** A policy engine that auto-approves low-risk gates (or auto-rejects on a timeout) is explicitly out of scope; it would build on this state machine.
- **Live-watcher steering during validation.** If, in practice, the live session is still open when the gate is reached, a future revision could let the in-session watcher surface the gate immediately rather than waiting for the reactive sweep.

## Next Steps

Run `/task 00016-FEATURE-manual-grader-approval-gate` to generate implementation tasks from this spec.
