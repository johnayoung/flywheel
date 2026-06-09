# Feature: Harness resilience — budget hygiene and clean crash retries

## Summary

Four small, surgical changes that came out of auditing the first end-to-end
phase run (`01-store-postgres`, 2026-05-25):

1. Raise the SDK turn cap to a true backstop value so per-task budgets don't
   silently undercut it.
2. Stop the `/task` skill from auto-recommending a transcript budget on
   every generated task.
3. Make `INTERNAL_ERROR` retry-eligible so crashes retry inside the same
   lifecycle instead of spawning a new one.
4. Drop the dead `implementation_notes` column from the persistence schema.

Together these prevent the failure mode we observed (three lifecycles for one
task, all `attempt #1`, all caused by a 40-turn SDK ceiling firing below an
80-turn per-task grader budget) and stop the misuse pattern at the source.

## Background

The first autonomous phase run completed successfully but with friction.
Concrete evidence in `.workflow/flywheel.sqlite`:

- Three distinct lifecycles for `pg-store-impl`, all `attempt_number=1`,
  `retries=0`. The first two finalized as `internal_error` with
  `"Claude Code returned an error result: Reached maximum number of turns (40)"`.
- `implementation_notes` is `NULL` on every lifecycle; a `grep -rn` finds zero
  readers outside the schema files and the two store `INSERT` statements.

Findings traced through code:

- `workflow.py:74` sets `DEFAULT_MAX_TURNS = 40`, applied to
  `ClaudeAgentOptions(max_turns=...)` at `workflow.py:297-302`.
- `grader_transcript.py:139` and `:149-150` show the transcript grader's
  `max_turns` counts the same unit as the SDK's `max_turns` — whichever is
  lower wins. With SDK=40 and grader=80, the SDK ceiling fires first.
- An SDK-cap breach finalizes as `INTERNAL_ERROR`. `harness.py:454` only
  retries `FAILED_VALIDATION`, so crashes exit the lifecycle terminal and the
  outer worker creates a fresh one.
- `.claude/commands/task.md:113` recommends `transcript max_turns=30` as a
  default grader in the `/task` skill template, propagating the misuse
  pattern into every generated task.

## Scope

### In Scope

- Worker default `--max-turns` raised to `500` (true backstop, not a per-task
  budget). Help text reflects the intent.
- `/task` skill no longer recommends a transcript-budget grader by default.
- `Lifecycle.is_retry_eligible` covers `INTERNAL_ERROR` under the same
  budget as `FAILED_VALIDATION`. Harness retries crashes inside the lifecycle.
- `implementation_notes` removed from both schema files and both stores.

### Out of Scope (deliberately deferred)

All of the following came up in the audit but are *not* in this MVP. They
become follow-up work if and when crashes prove to be a recurring problem in
practice. See "Future work" below for the full list.

- Session-id capture, persistence, or SDK `resume=` plumbing.
- Capturing `git diff` / `git status` snapshots on crash.
- Resume-context prompt preambles for retry attempts.
- Validation-retry working-tree reset (`git reset --hard` between attempts).
- New `flywheel.sandbox` helper module.
- New event kinds (`harness.resume_scheduled`).
- Forward-compatible invoker Protocol for OpenAI Codex.
- Bounded outer-worker retry budget.

### Tradeoffs accepted

- If a crash *does* still happen (e.g. transient API error), the next
  attempt's transcript will still look like "files mysteriously exist."
  That's worse for audit but works in practice. We bet the 500-turn cap
  makes this rare enough that engineering around it preemptively isn't worth
  the complexity.
- `implementation_notes` removal is a non-additive schema change. Flywheel
  doesn't own migrations; existing dev DBs can be wiped or
  manually `ALTER`'d.

## Requirements

### Functional Requirements

1. **FR-1: SDK cap is a backstop.** `workflow.py:DEFAULT_MAX_TURNS` set to
   `500`. `task-worker.sh --help` reflects the new default and help text
   describes it as a runaway-prevention backstop, not a per-task budget.
   - Acceptance: constant updated; help text updated; existing tests pass.

2. **FR-2: `/task` skill drops the default transcript grader.** The example
   grader block in `.claude/commands/task.md` no longer includes a
   `transcript` grader. The "Grader types" table still documents it as an
   opt-in option.
   - Acceptance: skill template no longer suggests adding a transcript
     grader to new tasks by default.

3. **FR-3: `INTERNAL_ERROR` is retry-eligible.**
   `Lifecycle.is_retry_eligible` returns `True` when
   `status in (FAILED_VALIDATION, INTERNAL_ERROR)` and
   `retries < max_retries`. The harness branch at `harness.py:454` extends
   to cover both statuses; the existing `harness.retry_scheduled` event is
   emitted in both cases (no new event kind).
   - Acceptance: harness tests cover crash-retry-eligible and
     crash-retries-exhausted paths; existing `failed_validation` retry path
     remains green.

4. **FR-4: `implementation_notes` removed.** Both canonical schema files,
   both store `INSERT` statements, and any dataclass field references are
   gone.
   - Acceptance: `grep -rn implementation_notes src/ docs/ tests/` returns
     nothing; full suite remains green.

### Non-Functional Requirements

1. **NFR-1: No new modules, no new dependencies, no new event kinds.** All
   changes are local edits to existing files.
2. **NFR-2: Module-purity invariants hold.** No changes to `flywheel.task`
   or `flywheel.lifecycle` beyond extending the existing
   `is_retry_eligible` predicate.

## Behavior Specification

### Happy path

- Task with no transcript grader runs to `done` in N iterations,
  N << 500. SDK cap never fires.

### Per-task budget exceeded

- Task explicitly opts into `transcript max_turns=20` and exceeds it.
  Transcript grader returns `failed`. Lifecycle goes
  `running -> validating -> failed_validation`. Harness retries under the
  existing budget. (Unchanged from today.)

### Genuine SDK crash

- Agent SDK errors (e.g. transient API failure, or the 500-turn backstop
  fires). Lifecycle goes `running -> internal_error`. Harness checks retry
  budget, emits `harness.retry_scheduled`, transitions back to `ready`,
  runs next attempt. Working tree is **not** reset between attempts (today's
  behavior preserved).

### Retry budget exhausted

- On the Nth crash where N exceeds `max_retries`. Lifecycle finalizes as
  `failed` with `"retries exhausted (N/N)"` and the worker does not
  re-select.

## Technical Context

### Affected files

- `src/flywheel/workflow.py` — `DEFAULT_MAX_TURNS = 500`; CLI help text.
- `src/flywheel/lifecycle.py` — extend `is_retry_eligible` to cover
  `INTERNAL_ERROR`.
- `src/flywheel/harness.py` — extend the retry branch at `:454` so
  `INTERNAL_ERROR` follows the same path as `FAILED_VALIDATION`.
- `src/flywheel/store_sqlite.py` — drop `implementation_notes` from INSERT
  and any conversion code.
- `src/flywheel/store_postgres.py` — same.
- `docs/persistence-schema.sql` — drop `implementation_notes` column.
- `docs/persistence-schema-postgres.sql` — drop `implementation_notes` column.
- `.claude/commands/task.md` — drop default transcript-grader recommendation.
- `task-worker.sh` — update help text default for `--max-turns`.

## Decisions log

- **2026-05-25** — Scope intentionally narrowed from an earlier broader
  draft. The earlier draft included session-id capture, crash snapshots,
  resume preambles, a `flywheel.sandbox` module, and forward-compatible
  Codex hooks. All deferred. Rationale: with a 500-turn backstop, crashes
  become rare; existing "files persist on disk" behavior handles the rare
  case well enough until proven otherwise. Engineering for the failure mode
  preemptively before it's a recurring problem is over-investment.
- **2026-05-25** — Worker `--max-turns` default committed to 500. High
  enough to be a true runaway-prevention backstop; finite to bound cost.
- **2026-05-25** — `implementation_notes` removed rather than retained for a
  hypothetical future use. Reintroduce when a real consumer needs it.
- **2026-05-25** — Per Claude Agent SDK docs, the SDK ceiling and the
  transcript-grader budget count the same unit (`AssistantMessage` per
  iteration, observed via `ResultMessage.num_turns`). Lower wins. Raising
  the SDK cap eliminates the silent-undercut footgun.

## Future work (parked)

If crashes become a recurring problem in practice, the following make a
natural follow-up spec:

- **Session-id capture + Claude `resume=`.** Persist `IterationResult.session_id`
  onto the attempt; pass it back on crash-retry invocations. Cheap if the SDK
  surface stays as-is, but adds a backend-divergence concern (Codex has a
  different resume model).
- **Crash snapshots + resume preamble.** Capture `git status`/`git diff` on
  crash, store on the attempt, summarize into the next attempt's prompt.
  Makes crash retries auditable instead of "files mysteriously exist."
- **Validation-retry clean slate.** `git reset --hard` + `git clean -fd`
  between `failed_validation` retries. Prevents bad code from leaking into
  the next attempt. Open question whether this is actually needed —
  validators catch the bad code anyway.
- **OpenAI Codex invoker.** Requires a backend-agnostic resume abstraction
  in the invoker Protocol. See audit research for the divergence between
  Claude SDK resume and OpenAI Responses/Conversations APIs.
- **Bounded outer-worker retry budget.** Today `task-worker.sh` will keep
  re-selecting a `RETRYABLE` task indefinitely. With FR-3, crash retries
  happen inside the lifecycle, narrowing the blast radius — but a true
  outer ceiling is still missing.
