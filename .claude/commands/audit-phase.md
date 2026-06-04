---
description: Audit flywheel's own execution of a completed phase to surface loop failures, misleads, and waste
---

Audit **flywheel itself**, not the work it produced. Read the SQLite store, worker logs, and task JSONs for the named phase, then surface where the loop misfired: crashes, retries, validation flaps, budget squeezes, grader disagreements, wasted lifecycles, ambiguous specs that confused the agent. Produce a findings doc that names what went wrong and why.

This is **not** `/review-phase` (which checks whether shipped work is correct). It is also **not** a place to propose new flywheel features — that belongs in `/propose-improvements` (a separate command that reads audits and proposes fixes). This command's job is observation and diagnosis only.

## INPUT

$ARGUMENTS

Accepted forms:
- `02-harness-resilience` — phase dir name (looked up under `.workflow/tasks/active/` then `archive/`)
- (no arg) — most recently archived phase

## CORE PRINCIPLE

**Every finding cites evidence and stops at diagnosis.** A `run_id`, an `events.id`, a log file:line, a grader result row — no prose without a pointer the reader can re-verify. Name what happened and why. Do not name what flywheel should have, do not propose new code, do not recommend features. If you find yourself writing "flywheel should add X", stop — that belongs in `/propose-improvements`.

## STEP 1: RESOLVE THE PHASE

```bash
# Active first, then archive
ARG="${1:-}"
if [[ -z "$ARG" ]]; then
  PHASE=$(ls -1 .workflow/tasks/archive/ 2>/dev/null | grep -E '^[0-9]+-' | sort | tail -1)
else
  PHASE="$ARG"
fi

# Locate the phase directory (active or archive)
for root in .workflow/tasks/active .workflow/tasks/archive; do
  [[ -d "$root/$PHASE" ]] && PHASE_DIR="$root/$PHASE" && break
done

# Task IDs in this phase
ls "$PHASE_DIR"/*.json
```

If the phase directory does not exist, stop and tell the user.

## STEP 2: CHECK FOR LOOP ACTIVITY

Before writing anything, check whether the loop actually ran for this phase:

```sql
SELECT COUNT(*) FROM lifecycles WHERE task_id IN (<phase task ids>);
```

If the count is `0` for every task — no lifecycles, no worker logs, no events — **do not invent findings**. Write a short note instead:

```markdown
## Phase audit: <phase>

**Source:** `.workflow/flywheel.sqlite`, `logs/worker/`, `<phase-dir>`
**Audited:** <date>
**Wall-clock window:** <earliest commit> -> <latest commit> (from `git log` over the phase task files)

## Verdict

**Nothing to audit. The loop did not run for this phase.**

<1-2 sentences: which tasks, zero telemetry, what the operator did instead — e.g. shipped by hand, ran outside the worker.>

## Evidence

- `sqlite3 ... "SELECT COUNT(*) FROM lifecycles WHERE task_id IN (...)"` -> `0`
- `ls logs/worker/<task_id>*` -> no files
- `git log --diff-filter=A -- '<phase-dir>/*.json'` -> <commit shape that proves bypass>

## Context for the bypass (optional, only if relevant)

<1 paragraph: why the loop was bypassed, if discoverable from commit messages or the task scope. Skip if not obvious.>
```

That is the entire audit. Stop. Do not pad with feature ideas, do not invent "recommendations" to fill the template. A bypassed phase produces a short note; the friction it surfaces (if any) is for `/propose-improvements` to act on, not this command.

## STEP 3: PULL THE EVIDENCE (only if loop activity exists)

The SQLite store at `.workflow/flywheel.sqlite` is the source of truth. Tables: `lifecycles`, `attempts`, `events`, `grader_results`. Schema: `src/flywheel/_schema/persistence-schema.sql`.

For each task `id` in the phase, query (use `sqlite3` directly):

```sql
-- All lifecycles for this task
SELECT run_id, status, retries, error, agent_output,
       timestamps_json, updated_at
FROM lifecycles
WHERE task_id = :task_id
ORDER BY updated_at;

-- All attempts, with wall-clock per attempt
SELECT run_id, number, outcome, error, started_at, ended_at,
       (julianday(ended_at) - julianday(started_at)) * 86400 AS seconds
FROM attempts
WHERE run_id IN (:run_ids)
ORDER BY started_at;

-- Event timeline (every harness.* kind)
SELECT id, run_id, attempt_number, ts, kind, payload_json
FROM events
WHERE run_id IN (:run_ids)
ORDER BY ts;

-- Grader receipts
SELECT run_id, attempt_number, ordinal, grader_type, grader_name,
       passed, duration_ms, payload_json
FROM grader_results
WHERE run_id IN (:run_ids)
ORDER BY run_id, attempt_number, ordinal;
```

Event kinds emitted today: `harness.attempt_started`, `harness.iteration_completed`, `harness.attempt_finalized`, `harness.crash`, `harness.blocked`, `harness.budget_exceeded`, `harness.retry_scheduled`, `harness.protocol_failure`.

Also collect:

```bash
# Worker log files for each task in the phase
ls logs/worker/<task_id>_*.log

# What code shipped during the phase wall-clock window
git log --since="<earliest_started_at>" --until="<latest_ended_at>" --oneline

# Original task spec the agent was given
cat "$PHASE_DIR/<task_id>.json"
```

## STEP 4: CLASSIFY FINDINGS

For each task, decide which buckets apply. **Skip tasks with no findings — they get a one-line "clean" entry, not a section.**

| Bucket                  | Detection                                                                                | Why it matters                                              |
| ----------------------- | ---------------------------------------------------------------------------------------- | ----------------------------------------------------------- |
| Multi-lifecycle         | `>1` rows in `lifecycles` for the same `task_id`                                         | Outer-worker retry — inner harness budget was the wrong size or crashes weren't caught |
| In-lifecycle retry      | `retries > 0` on the final lifecycle row                                                 | Validation or crash retry — what failed first?              |
| SDK crash               | `events.kind = 'harness.crash'`                                                          | Backstop fired or SDK errored. Reproducible? Transient?     |
| Budget squeeze          | `events.kind = 'harness.budget_exceeded'`                                                | Per-task grader budget or transcript cap hit                |
| Grader flap             | Same `(run_id, ordinal)` grader passed in one attempt, failed in another                 | Test flake or state leaking between attempts                |
| Agent-vs-grader miss    | `attempts.outcome = 'verify'` but graders disagreed                                      | Agent's claim was wrong — prompt or envelope problem        |
| Long wall-clock         | Attempt `seconds` > some threshold (use phase median × 3)                                | Token waste, infinite loops, slow graders                   |
| Blocked                 | `events.kind = 'harness.blocked'`                                                        | Operator-intervention required                              |
| Protocol failure        | `events.kind = 'harness.protocol_failure'`                                               | Envelope/lifecycle bug                                      |
| Spec ambiguity (manual) | Agent transcript shows clarifying loops, re-reads, false starts (read log if present)    | Task spec under-specified — visible in the log              |

For each finding, pull the **exact** evidence: the row, the event payload JSON snippet, the log lines, the grader payload. Quote it verbatim in the report — do not paraphrase.

## STEP 5: LOOP-PATH MARKER RE-CHECK (FR-6)

The archive gate (`archive_completed_phases`, spec `00017-FEATURE-in-loop-verification-gate.md`) refuses to move a loop-path-marked phase into `archive/` without a DONE `in-loop-verification` task or a valid `loop-path-exempt.md` opt-out. The audit's job here is the coverage check on the gate's trigger: re-derive the marker against the **archived** phase's recorded base and surface (a) a watched-symbol diff that slipped through without coverage, and (b) an opt-out whose "no new path" claim is contradicted by the diff.

Do NOT guess signals from prose. The phase's `.loop-base` dotfile travels with the phase into `archive/` (it's a committed file inside the phase dir), so `git diff <.loop-base> HEAD` is the same input the gate would have seen. Re-derive mechanically by running `flywheel.loop_path_marker.detect_loop_path_signals` over `flywheel.workflow.phase_diff_vs_base`, and load the opt-out via `flywheel.workflow.load_loop_path_optout`:

```bash
uv run python - <<PY
import json
from pathlib import Path
from flywheel.workflow import (
    phase_diff_vs_base,
    load_loop_path_optout,
    read_phase_base,
    LoopPathOptOutError,
)
from flywheel.loop_path_marker import detect_loop_path_signals

phase_dir = Path("$PHASE_DIR")
diff = phase_diff_vs_base(Path("."), phase_dir)
signals = sorted(s.value for s in detect_loop_path_signals(diff))
try:
    opt = load_loop_path_optout(phase_dir)
    opt_repr = None if opt is None else {"phase": opt.phase, "author": opt.author, "reason": opt.reason}
    opt_error = None
except LoopPathOptOutError as e:
    opt_repr = None
    opt_error = str(e)
print(json.dumps({
    "base": read_phase_base(phase_dir),
    "signals": signals,
    "opt_out": opt_repr,
    "opt_out_error": opt_error,
}, indent=2))
PY
```

Then check the phase's tasks for a DONE `in-loop-verification`-tagged lifecycle (the same surface `_loop_path_gate_satisfied` uses):

```bash
# A task carries the marker iff "in-loop-verification" is in its tags JSON.
for f in "$PHASE_DIR"/*.json; do
  python -c "import json,sys; t=json.load(open(sys.argv[1])); print(sys.argv[1], 'in-loop-verification' in (t.get('tags') or []))" "$f"
done
```

```sql
-- DONE lifecycles for the candidate task ids from the step above
SELECT task_id, run_id, status
FROM lifecycles
WHERE task_id IN (<verify-tagged task ids>)
  AND status = 'done';
```

Emit findings:

- **FR-6a — Loop-path diff archived with no coverage.** `signals` is non-empty AND no DONE `in-loop-verification` task AND `opt_out` is `null`. A `LoopPathOptOutError` (malformed opt-out shipped into archive) is itself an FR-6a finding: the artifact is unreadable, so the claim cannot be honored. Cite the base SHA, the tripped signal names, and the diff hunks that produced each signal.
- **FR-6b — Opt-out covers a watched-symbol diff.** `opt_out` is non-null AND `signals` is non-empty. Quote the opt-out's `reason` verbatim, list the tripped signals, and cite the diff hunk for each. The audit surfaces the falsifiable contradiction; it does not overrule the opt-out.

Skip both findings when `signals` is empty (pure refactor — e.g. a docstring/rename inside a watched file produces no added watched symbol) OR when a DONE `in-loop-verification` task exists. Acceptance to encode: a mis-tagged opt-out over a diff that adds a `Status` member yields an FR-6b finding; a correct opt-out over a pure refactor (no added watched symbol) yields none.

Record both as **phase-level** findings under their own section in the report (Step 7), not under any individual task.

## STEP 6: CROSS-TASK PATTERNS

After per-task analysis, scan for:

- **Recurring error strings** — same `error` text across multiple `lifecycles` or `attempts` rows
- **Same crash payload** — identical `harness.crash` `payload_json` across tasks (likely an SDK/infra issue, not the agent)
- **Prerequisite cascade** — task A failing held up task B's start
- **Worker idle gaps** — large `updated_at` gaps between consecutive lifecycles (operator paused? deadlock?)
- **Grader-class waste** — one grader type accounts for most of the retried attempts

## STEP 7: WRITE THE REPORT

Output goes to `.workflow/audits/<phase>.md`. Create the dir if needed (`mkdir -p .workflow/audits`). Audits are committed, not gitignored — they're the historical record of flywheel-on-flywheel learnings.

```markdown
# Phase audit: <phase>

**Source:** `.workflow/flywheel.sqlite`, `logs/worker/`, `<phase-dir>`
**Audited:** <today's date>
**Wall-clock window:** <earliest started_at> -> <latest ended_at>

## Summary

| Metric                              | Value |
| ----------------------------------- | ----- |
| Tasks in phase                      | N     |
| Tasks reaching DONE                 | N     |
| Total lifecycles                    | N     |
| Tasks requiring >1 lifecycle        | N     |
| In-lifecycle retries                | N     |
| `harness.crash` events              | N     |
| `harness.budget_exceeded` events    | N     |
| Median attempt wall-clock (seconds) | N     |

One-line health verdict: _"Phase ran clean / friction in X areas / loop misbehaved"._

## Per-task findings

### clean
- `task-id-1` — 1 lifecycle, 1 attempt, all graders pass. (`run_id: ...`)
- `task-id-2` — 1 lifecycle, 1 attempt, all graders pass. (`run_id: ...`)

### `<task-id>` — <bucket(s) that apply>

**What happened**
- Lifecycle 1 `run-abc` (status: `failed`): <evidence>
- Lifecycle 2 `run-def` (status: `done`): <evidence>

**Evidence**
- `events.id=42` `harness.crash` payload: `<verbatim json snippet>`
- `attempts(run-abc, 1).error`: `"<verbatim error>"`
- `logs/worker/<task>_<sha>_<ts>.log:120-140`: `<excerpt>`

**Diagnosis** (what part of the loop caused this — not what to add)
- <1-2 sentences naming the loop mechanism that failed and why, tied directly to the evidence above. Example: "The harness only emits `harness.crash` when its own handler catches the exception; SIGINT propagated through `asyncio.run` and killed the process before `_run_attempt` reached its finalizer." Stop there. Do not say "we should install X".>

## Loop-path marker re-check (FR-6)

- **Base SHA:** `<.loop-base contents>`
- **Re-derived signals:** `[signal_a, signal_b]` (or `[]` for "clean")
- **`in-loop-verification` task:** `<task-id>` lifecycle `<run_id>` status `done` (or "absent")
- **Opt-out:** `<phase / author / reason>` (or "absent" / "malformed: <error>")

**Finding** (only when one applies; omit the section if signals is empty AND a DONE verify task or valid opt-out is present)

- FR-6a — loop-path diff archived with no DONE `in-loop-verification` task and no opt-out. Signals: `[...]`. Diff hunks: `<file:line>` ...
- FR-6b — opt-out covers a watched-symbol diff. Opt-out `reason` (verbatim): `"..."`. Signals: `[...]`. Diff hunks: `<file:line>` ...

## Cross-task patterns

- <pattern>: <evidence: list of run_ids/event ids>
```

## STEP 8: PRESENT

After writing the file:

```
## Audit complete

Wrote `.workflow/audits/<phase>.md`.

**Headline:** <one-sentence verdict — what the loop did, not what it should do>

**Top friction observed:**
- <bucket>: <count> tasks affected
- <bucket>: <count> tasks affected

Open the file for the evidence trail. Run `/propose-improvements` if you want to turn these findings into action.
```

## RULES

1. **Every finding cites a row, event id, or log file:line.** No unsupported claims.
2. **Skip clean tasks.** A one-line "clean" entry per task is enough; no section.
3. **Audit the loop, not the work.** "Graders were right to reject this" is not a finding. "Grader rejected, agent retried with the same approach, no resume context" is.
4. **Distinguish flywheel bugs from agent mistakes.** A confused agent on an under-specified task is a flywheel issue (the template is loose); a clean spec where the agent shipped wrong code is not (that's `/review-phase`'s job). Naming the issue is the audit; designing the fix is not.
5. **No proposals, no recommendations, no feature ideas.** Stop at diagnosis. If a finding makes you want to write "flywheel should have X" or "add a Y subcommand" or "the template should require Z" — stop. That is `/propose-improvements` territory. The audit produces evidence; the next command produces action.
6. **Quote, do not paraphrase, evidence.** JSON payloads and error messages stay verbatim.
7. **A bypassed phase gets a short note, not a long doc.** If the loop did not run, write the short verdict from Step 2 and stop. Do not pad with feature ideas to fill the template.
8. **No emojis.**
