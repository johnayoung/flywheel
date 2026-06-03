---
description: Turn phase-audit findings into ranked, scoped improvement proposals for flywheel, handing each off to /define or /task
---

Read one or more phase audits and turn their evidence into **ranked, scoped proposals** for improving flywheel itself. Each proposal traces back to a cited audit finding, names the outcome (not the implementation), and ends in a concrete handoff: `/define` for a feature that needs discovery, `/task` for a fix that is already well understood, or an explicit "accept — do not fix".

This is the **action** half of the audit pipeline. `/audit-phase` produces evidence and stops at diagnosis; this command reads that evidence and proposes what to do about it. It does **not** write specs (that is `/define`), does **not** write task JSON (that is `/task`), and does **not** write loop code. It produces a proposals doc and the next command to run.

## INPUT

$ARGUMENTS

Accepted forms:
- `17-manual-grader-approval-gate` — phase name; reads `.workflow/audits/<phase>.md`
- `.workflow/audits/<phase>.md` — explicit audit path
- `all` — synthesize across every audit in `.workflow/audits/` (use for recurring-pattern proposals)
- (no arg) — most recently modified audit in `.workflow/audits/`

## CORE PRINCIPLE

**Every proposal cites an audit finding, and every proposal ends in a handoff.** No floating ideas, no "it would be nice if" — if a proposal does not trace to a finding (which itself cites a `run_id` / `events.id` / log line / grader row), it does not belong here. Propose the **outcome**, not the code. Ranking by leverage is part of the job: recommending "accept, do not fix" for a one-off is a valid, valuable proposal.

## STEP 1: RESOLVE THE AUDIT(S)

```bash
ARG="${1:-}"
mkdir -p .workflow/proposals

if [[ "$ARG" == "all" ]]; then
  AUDITS=$(ls -1 .workflow/audits/*.md 2>/dev/null)
elif [[ -f "$ARG" ]]; then
  AUDITS="$ARG"
elif [[ -n "$ARG" && -f ".workflow/audits/$ARG.md" ]]; then
  AUDITS=".workflow/audits/$ARG.md"
elif [[ -z "$ARG" ]]; then
  AUDITS=$(ls -1t .workflow/audits/*.md 2>/dev/null | head -1)
fi

echo "Audits in scope:"; echo "$AUDITS"
# Always list siblings — recurrence across phases raises a proposal's leverage.
echo "All audits on record:"; ls -1 .workflow/audits/*.md 2>/dev/null
```

If no audit file resolves, stop and tell the user to run `/audit-phase <phase>` first. Do not invent findings from the raw store — this command consumes audits, it does not produce them.

Read each in-scope audit in full. Read the sibling audits' headlines too: a finding that recurs across phases is higher-leverage than a one-off and must be flagged as such.

## STEP 2: CHECK FOR ACTIONABLE CONTENT

An audit may be a clean run or a bypassed-phase note. If it contains no findings and no observations that warrant action, **do not invent proposals**. Write the short note and stop:

```markdown
# Improvement proposals: <phase>

**Source audit:** `.workflow/audits/<phase>.md`
**Proposed:** <date>

## Verdict

**No action proposed.** The audit recorded a clean run with no friction worth a fix.

<1-2 sentences naming what the audit found — e.g. "10/10 tasks first-pass DONE; the only observations were inherent rate-limiting and a deliberate single-worker run.">
```

That is the entire output. Stop. Do not pad an empty audit into a backlog.

## STEP 3: EXTRACT FINDINGS

Parse each in-scope audit into a flat list of findings and observations. For each, capture verbatim:

- The finding label / bucket (e.g. "Multi-lifecycle", "No worker logs on disk", "Gate never exercised in-loop").
- Its evidence pointer(s) exactly as the audit cites them (`run_id`, `events.id`, log `file:line`, grader row, `COUNT(*) -> N`).
- Whether the same finding appears in any sibling audit (search the others for the bucket / error string / event kind). Note the recurrence count.

A finding with no re-verifiable evidence pointer is not eligible — skip it and note that the audit was thin there.

## STEP 4: CLASSIFY EACH FINDING

Assign each finding a **disposition** and a **leverage score**.

Disposition (what kind of action, if any):

| Disposition          | When                                                                 | Handoff        |
| -------------------- | -------------------------------------------------------------------- | -------------- |
| Loop bug             | The loop did the wrong thing (crash uncaught, bad transition, flap)  | `/define` or `/task` |
| Template tightening  | Task/spec template let an under-specified task through               | `/task` (edit the template/command) |
| Observability gap    | The loop ran fine but left no evidence to audit later                | `/define` or `/task` |
| New capability       | The friction needs a feature that does not exist yet                 | `/define`      |
| Accept / do not fix  | One-off, inherent cost, or cheaper to tolerate than to fix           | none           |

Leverage score (rank order, not a number for its own sake):

- **Recurrence** — appears in N audits. Cross-phase beats single-phase.
- **Blast radius** — how many tasks/runs the friction touched (cite the count from the audit).
- **Cost of inaction** — wasted tokens, lost auditability, silent wrong results.
- **Fix cost** — rough size of the change. A cheap fix to a recurring annoyance outranks an expensive fix to a one-off.

## STEP 5: DRAFT PROPOSALS

**Cluster related findings into one proposal.** One proposal per coherent improvement, not one per symptom. Three findings that all stem from "the worker does not persist transcripts" are one proposal, not three.

For each proposal, write:

- **Problem** — the finding(s), with the audit's evidence pointers quoted verbatim.
- **Proposed outcome** — what should be true after the fix, stated as an observable change in loop behavior. NOT an implementation. ("An `AWAITING_APPROVAL` lifecycle is exercised end-to-end by the worker at least once" — not "add a fixture that calls resolve_manual_approval".)
- **Disposition + handoff** — which next command advances it (`/define <name>` or `/task <free text or spec>`), or "accept".
- **Leverage** — the rank rationale (recurrence, blast radius, fix cost).

Stop at the outcome. If you find yourself specifying functions, columns, or signatures, you have crossed into `/define`/`/task` territory — cut it.

## STEP 6: RANK AND CONFIRM

Present the proposals ranked by leverage (highest first), each one paragraph with its handoff. Then use `AskUserQuestion` to let the operator decide disposition:

- One multi-select question: **which proposals to advance** (options = the proposal titles; the operator may pick several, or none).
- For each advanced proposal where the path is genuinely ambiguous, one follow-up question: **`/define` (feature, needs discovery) vs `/task` (scoped, ready)**. Skip the follow-up when the disposition already makes the path obvious.

Do not advance anything the operator did not pick. "Advance none" is a valid outcome — the doc still records the analysis.

## STEP 7: WRITE THE PROPOSALS DOC

Output goes to `.workflow/proposals/<phase>.md` (or `.workflow/proposals/cross-phase-<date>.md` for an `all` run). Committed, not gitignored — proposals are the record of what the audits drove.

```markdown
# Improvement proposals: <phase>

**Source audit(s):** `.workflow/audits/<phase>.md` (+ siblings consulted for recurrence)
**Proposed:** <today's date>

## Summary

| Metric                         | Value |
| ------------------------------ | ----- |
| Findings reviewed              | N     |
| Proposals (clustered)          | N     |
| Advancing via `/define`        | N     |
| Advancing via `/task`          | N     |
| Accepted (no fix)              | N     |

## Proposals (ranked by leverage)

### P1 — <title>  [disposition]

**Problem**
- <finding, with verbatim evidence pointer from the audit: `run_id` / `events.id` / log `file:line` / `COUNT(*) -> N`>
- Recurrence: <appears in phases X, Y / single-phase>

**Proposed outcome**
- <observable change in loop behavior — not an implementation>

**Handoff:** `/define <name>` | `/task <description>` | accept — do not fix
**Leverage:** <recurrence × blast radius × cost-of-inaction vs fix-cost, one line>
**Operator decision:** <advance / defer / accept — filled from STEP 6>

### P2 — <title>  [disposition]
...

## Not proposed (findings reviewed, no action)

- <finding> — <why no action: one-off / inherent / below the line>
```

## STEP 8: PRESENT AND HAND OFF

```
## Proposals ready

Wrote `.workflow/proposals/<phase>.md`.

**Headline:** <one sentence — e.g. "2 proposals: 1 observability fix to advance, 1 inherent cost to accept">

**Advancing:**
- P1 <title> -> run `/define <name>`
- P3 <title> -> run `/task <description>`

**Accepted (no fix):**
- P2 <title> — <one-line reason>

Run the handoff command for each advancing proposal when ready.
```

## RULES

1. **Every proposal traces to a cited audit finding.** If it does not, it does not belong here. The audit owns the evidence; this command reuses its pointers verbatim.
2. **Propose outcomes, not implementations.** No function names, no columns, no signatures. The change is described as observable loop behavior. Designing it is `/define`'s and `/task`'s job.
3. **Cluster, do not enumerate.** One proposal per coherent improvement. Symptoms of one root cause collapse into one proposal.
4. **Rank by leverage; recurrence beats one-offs.** A finding that shows up across multiple audits outranks a single-phase blip. Cheap fixes to recurring friction rank above expensive fixes to one-offs.
5. **"Accept — do not fix" is a real proposal.** Inherent costs (rate-limiting, a deliberate single-worker run) and one-offs get named and dismissed with a reason. Do not manufacture fixes for them.
6. **Do not invent work to fill the doc.** A clean audit yields the STEP 2 short note. Few or zero proposals is a correct result.
7. **Features go through `/define`, not ad hoc.** New capability proposals hand off to `/define` so they land as a `.workflow/specs/` spec; scoped fixes hand off to `/task`. This command writes neither.
8. **The operator owns prioritization.** Use `AskUserQuestion`; advance only what they pick. Record the analysis regardless of what they advance.
9. **No emojis.**
