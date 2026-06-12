---
name: fw-retro
description: Audit how the flywheel loop executed a completed phase to surface failures, misleads, and waste
argument-hint: [phase-dir-name, task id, or empty for the most recent]
---
<!-- managed-by: flywheel init -->

Audit **the loop's execution**, not the work it produced. Read the run history for the named phase (or tasks), then surface where the loop misfired: crashes, retries, validation flaps, budget squeezes, grader disagreements, wasted runs, ambiguous task specs that confused the agent. Produce a findings doc that names what went wrong and why.

This is observation and diagnosis only. Proposing fixes is `/fw-improve`'s job; checking whether the shipped work is correct is a code review, not this.

## INPUT

$ARGUMENTS

Accepted forms:
- A phase directory name (looked up under `__FW_TASKS_DIR__/active/` then `__FW_TASKS_DIR__/archive/`)
- A task id (audit just that task's runs)
- (no arg) -- the most recently archived phase, else the most recently finished tasks from `flywheel history`

## CORE PRINCIPLE

**Every finding cites evidence and stops at diagnosis.** A `run_id`, an audit-stream record, a transcript line, a grader receipt -- no prose without a pointer the reader can re-verify. Name what happened and why. Do not propose features or fixes; if you find yourself writing "flywheel should add X", stop -- that belongs in `/fw-improve`.

## STEP 1: RESOLVE THE SCOPE

```bash
ARG="${1:-}"
if [ -z "$ARG" ]; then
  PHASE=$(ls -1 __FW_TASKS_DIR__/archive/ 2>/dev/null | grep -E '^[0-9]+-' | sort | tail -1)
else
  PHASE="$ARG"
fi

# Locate the phase directory (active or archive)
for root in __FW_TASKS_DIR__/active __FW_TASKS_DIR__/archive; do
  [ -d "$root/$PHASE" ] && PHASE_DIR="$root/$PHASE" && break
done

# Task ids in this phase (directory work source: one JSON per task)
ls "$PHASE_DIR"/*.json 2>/dev/null
```

If no phase directory resolves, treat the argument as a task id, or fall back to the most recently finished tasks:

```bash
flywheel history --limit 10
```

## STEP 2: CHECK FOR LOOP ACTIVITY

Before writing anything, check whether the loop actually ran for this scope:

```bash
flywheel history --json
```

Filter the JSON for the in-scope task ids. If none of them appear -- no runs, no transcripts -- **do not invent findings**. Write a short note instead:

```markdown
# Loop retro: <scope>

**Audited:** <date>

## Verdict

**Nothing to audit. The loop did not run for this scope.**

<1-2 sentences: which tasks, zero run history, what the operator did instead -- e.g. shipped by hand, ran outside the worker.>

## Evidence

- `flywheel history --json` -> no entries for <task ids>
- `git log --diff-filter=A -- '<phase-dir>/*.json'` -> <commit shape that proves bypass>
```

That is the entire retro. Stop. Do not pad with ideas to fill the template.

## STEP 3: PULL THE EVIDENCE (only if loop activity exists)

Flywheel's store is the source of truth; read it through the CLI (never assume a backend or schema):

```bash
# One line per task with run counts and totals
flywheel history --json

# Full detail for each in-scope task: latest run, attempts, grader
# receipts, final agent output, and every related (prior) run
flywheel show <task-id> --json

# Drill into a specific prior run by run_id
flywheel show <run-id> --json

# The totally-ordered audit record stream for a run (transitions,
# attempts, retries, blocks, grader evaluations, operator commands)
flywheel audit <run-id>
```

Also collect, where present:

```bash
# Per-run transcript (the agent's full message stream)
ls __FW_LOGS_DIR__/runs/<run-id>.jsonl

# What code shipped during the scope's wall-clock window
git log --since="<earliest started_at>" --until="<latest finished_at>" --oneline

# The original task definition the agent was given
cat "$PHASE_DIR/<task-id>.json"
```

## STEP 4: CLASSIFY FINDINGS

For each task, decide which buckets apply. **Skip tasks with no findings -- they get a one-line "clean" entry, not a section.**

| Bucket                  | Detection                                                                  | Why it matters                                              |
| ----------------------- | --------------------------------------------------------------------------- | ----------------------------------------------------------- |
| Multi-run               | `flywheel show <task-id> --json` lists related runs                         | Outer retry -- the first run died or failed terminally       |
| In-run retry            | `retries > 0` on the run                                                    | Validation or crash retry -- what failed first?              |
| Crash / internal error  | crash or internal-error records in `flywheel audit <run-id>`                | Backstop fired or the agent SDK errored. Reproducible?      |
| Budget squeeze          | budget-exceeded records in the audit stream / transcript-grader failures    | Turn, token, or wall-time cap hit                           |
| Grader flap             | Same grader passed in one attempt, failed in another                        | Test flake or state leaking between attempts                |
| Agent-vs-grader miss    | Agent reported done but graders disagreed                                   | The agent's claim was wrong -- prompt or task-spec problem   |
| Long wall-clock         | Attempt duration far above the scope median                                 | Token waste, loops, slow graders                            |
| Blocked / parked        | Interrupted-with-requires or awaiting-approval records in the audit stream  | Operator intervention was required                          |
| Spec ambiguity (manual) | Transcript shows clarifying loops, re-reads, false starts                   | Task spec under-specified -- visible in the transcript       |

For each finding, pull the **exact** evidence: the run id, the audit record, the grader receipt, the transcript excerpt. Quote it verbatim -- do not paraphrase.

## STEP 5: CROSS-TASK PATTERNS

After per-task analysis, scan for:

- **Recurring error strings** -- same error text across multiple runs
- **Same crash shape** -- identical failure across tasks (likely infra, not the agent)
- **Prerequisite cascade** -- task A failing held up task B's start
- **Idle gaps** -- large gaps between consecutive runs (operator paused? deadlock?)
- **Grader-class waste** -- one grader type accounts for most retried attempts

## STEP 6: WRITE THE REPORT

Output goes to `__FW_AUDITS_DIR__/<scope>.md`. Create the dir if needed. Audits are committed, not gitignored -- they are the historical record.

```markdown
# Loop retro: <scope>

**Audited:** <today's date>
**Wall-clock window:** <earliest started_at> -> <latest finished_at>

## Summary

| Metric                              | Value |
| ----------------------------------- | ----- |
| Tasks in scope                      | N     |
| Tasks reaching done                 | N     |
| Total runs                          | N     |
| Tasks requiring >1 run              | N     |
| In-run retries                      | N     |
| Crash / internal-error records      | N     |
| Median attempt wall-clock (seconds) | N     |

One-line health verdict: _"Ran clean / friction in X areas / loop misbehaved"._

## Per-task findings

### clean
- `task-id-1` -- 1 run, 1 attempt, all graders pass. (`run_id: ...`)

### `<task-id>` -- <bucket(s) that apply>

**What happened**
- Run `run-abc` (status: `failed`): <evidence>
- Run `run-def` (status: `done`): <evidence>

**Evidence**
- `flywheel audit run-abc` record: `<verbatim snippet>`
- `flywheel show run-abc --json` attempt 1 error: `"<verbatim error>"`
- `__FW_LOGS_DIR__/runs/run-abc.jsonl` (excerpt): `<excerpt>`

**Diagnosis** (what part of the loop caused this -- not what to add)
- <1-2 sentences naming the loop mechanism that failed and why, tied to the evidence above. Stop there. Do not say "we should install X".>

## Cross-task patterns

- <pattern>: <evidence: list of run_ids / audit records>
```

## STEP 7: PRESENT

After writing the file:

```
## Retro complete

Wrote `__FW_AUDITS_DIR__/<scope>.md`.

**Headline:** <one-sentence verdict -- what the loop did, not what it should do>

**Top friction observed:**
- <bucket>: <count> tasks affected

Open the file for the evidence trail. Run /fw-improve to turn these findings into action.
```

## RULES

1. **Every finding cites a run id, audit record, or transcript line.** No unsupported claims.
2. **Skip clean tasks.** A one-line "clean" entry per task is enough; no section.
3. **Audit the loop, not the work.** "Graders were right to reject this" is not a finding. "Grader rejected, agent retried with the same approach, no resume context" is.
4. **Distinguish loop friction from agent mistakes.** A confused agent on an under-specified task is loop friction (the task template is loose); a clean spec where the agent shipped wrong code is a code-review matter, not a retro finding.
5. **No proposals, no recommendations, no feature ideas.** Stop at diagnosis. That is /fw-improve territory.
6. **Quote, do not paraphrase, evidence.** Errors and payloads stay verbatim.
7. **A bypassed scope gets a short note, not a long doc.**
8. **No emojis.**
