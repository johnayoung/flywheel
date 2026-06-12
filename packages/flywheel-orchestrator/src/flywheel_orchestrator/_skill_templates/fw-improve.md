---
name: fw-improve
description: Turn fw-retro findings into ranked, scoped improvement proposals, handing each off to fw-spec or fw-plan
argument-hint: [retro name, path, "all", or empty for the most recent]
---
<!-- managed-by: flywheel init -->

Read one or more loop retros and turn their evidence into **ranked, scoped proposals** for improving how this repo runs its flywheel loop. Each proposal traces back to a cited retro finding, names the outcome (not the implementation), and ends in a concrete handoff: `/fw-spec` for a change that needs discovery, `/fw-plan` for a fix that is already well understood, or an explicit "accept -- do not fix".

This is the **action** half of the retro pipeline. `/fw-retro` produces evidence and stops at diagnosis; this command reads that evidence and proposes what to do about it. It does **not** write specs (that is `/fw-spec`), does **not** write tasks (that is `/fw-plan`), and does **not** write code. It produces a proposals doc and the next command to run.

## INPUT

$ARGUMENTS

Accepted forms:
- `<scope>` -- retro name; reads `__FW_AUDITS_DIR__/<scope>.md`
- `__FW_AUDITS_DIR__/<scope>.md` -- explicit retro path
- `all` -- synthesize across every retro in `__FW_AUDITS_DIR__/` (use for recurring-pattern proposals)
- (no arg) -- most recently modified retro in `__FW_AUDITS_DIR__/`

## CORE PRINCIPLE

**Every proposal cites a retro finding, and every proposal ends in a handoff.** No floating ideas, no "it would be nice if" -- if a proposal does not trace to a finding (which itself cites a run id / audit record / transcript line / grader receipt), it does not belong here. Propose the **outcome**, not the code. Ranking by leverage is part of the job: recommending "accept, do not fix" for a one-off is a valid, valuable proposal.

## STEP 1: RESOLVE THE RETRO(S)

```bash
ARG="${1:-}"
mkdir -p __FW_PROPOSALS_DIR__

if [ "$ARG" = "all" ]; then
  RETROS=$(ls -1 __FW_AUDITS_DIR__/*.md 2>/dev/null)
elif [ -f "$ARG" ]; then
  RETROS="$ARG"
elif [ -n "$ARG" ] && [ -f "__FW_AUDITS_DIR__/$ARG.md" ]; then
  RETROS="__FW_AUDITS_DIR__/$ARG.md"
else
  RETROS=$(ls -1t __FW_AUDITS_DIR__/*.md 2>/dev/null | head -1)
fi

echo "Retros in scope:"; echo "$RETROS"
# Always list siblings -- recurrence across scopes raises a proposal's leverage.
echo "All retros on record:"; ls -1 __FW_AUDITS_DIR__/*.md 2>/dev/null
```

If no retro file resolves, stop and tell the user to run `/fw-retro <scope>` first. Do not invent findings from the raw store -- this command consumes retros, it does not produce them.

Read each in-scope retro in full. Read the sibling retros' headlines too: a finding that recurs across scopes is higher-leverage than a one-off and must be flagged as such.

## STEP 2: CHECK FOR ACTIONABLE CONTENT

A retro may be a clean run or a bypassed-scope note. If it contains no findings that warrant action, **do not invent proposals**. Write the short note and stop:

```markdown
# Improvement proposals: <scope>

**Source retro:** `__FW_AUDITS_DIR__/<scope>.md`
**Proposed:** <date>

## Verdict

**No action proposed.** The retro recorded a clean run with no friction worth a fix.

<1-2 sentences naming what the retro found.>
```

That is the entire output. Stop. Do not pad an empty retro into a backlog.

## STEP 3: EXTRACT FINDINGS

Parse each in-scope retro into a flat list of findings. For each, capture verbatim:

- The finding label / bucket (e.g. "Multi-run", "Grader flap", "Spec ambiguity").
- Its evidence pointer(s) exactly as the retro cites them (run id, audit record, transcript line, grader receipt).
- Whether the same finding appears in any sibling retro (search the others for the bucket / error string). Note the recurrence count.

A finding with no re-verifiable evidence pointer is not eligible -- skip it and note that the retro was thin there.

## STEP 4: CLASSIFY EACH FINDING

Assign each finding a **disposition** and a **leverage rank**.

Disposition (what kind of action, if any):

| Disposition          | When                                                                  | Handoff        |
| -------------------- | --------------------------------------------------------------------- | -------------- |
| Task-spec tightening | An under-specified task confused the agent                            | `/fw-plan` (re-issue with a tighter brief) |
| Grader fix           | A flaky or wrong grader wasted runs                                   | `/fw-plan`     |
| Workflow change      | How tasks are sized, phased, or sequenced caused friction             | `/fw-spec` or `/fw-plan` |
| New capability       | The friction needs something that does not exist yet                  | `/fw-spec`     |
| Accept / do not fix  | One-off, inherent cost, or cheaper to tolerate than to fix            | none           |

Leverage rank:

- **Recurrence** -- appears in N retros. Cross-scope beats single-scope.
- **Blast radius** -- how many tasks/runs the friction touched (cite the count).
- **Cost of inaction** -- wasted tokens, lost auditability, silent wrong results.
- **Fix cost** -- rough size of the change. A cheap fix to recurring friction outranks an expensive fix to a one-off.

## STEP 5: DRAFT PROPOSALS

**Cluster related findings into one proposal.** One proposal per coherent improvement, not one per symptom.

For each proposal, write:

- **Problem** -- the finding(s), with the retro's evidence pointers quoted verbatim.
- **Proposed outcome** -- what should be true after the fix, stated as an observable change. NOT an implementation.
- **Disposition + handoff** -- which next command advances it (`/fw-spec <name>` or `/fw-plan <description>`), or "accept".
- **Leverage** -- the rank rationale (recurrence, blast radius, fix cost).

Stop at the outcome. If you find yourself specifying functions or file diffs, you have crossed into `/fw-spec`/`/fw-plan` territory -- cut it.

## STEP 6: RANK AND CONFIRM

Present the proposals ranked by leverage (highest first), each one paragraph with its handoff. Then use AskUserQuestion to let the operator decide disposition:

- One multi-select question: **which proposals to advance** (options = the proposal titles; the operator may pick several, or none).
- For each advanced proposal where the path is genuinely ambiguous, one follow-up question: **`/fw-spec` (needs discovery) vs `/fw-plan` (scoped, ready)**. Skip the follow-up when the disposition already makes the path obvious.

Do not advance anything the operator did not pick. "Advance none" is a valid outcome -- the doc still records the analysis.

## STEP 7: WRITE THE PROPOSALS DOC

Output goes to `__FW_PROPOSALS_DIR__/<scope>.md` (or `__FW_PROPOSALS_DIR__/cross-scope-<date>.md` for an `all` run). Committed, not gitignored.

```markdown
# Improvement proposals: <scope>

**Source retro(s):** `__FW_AUDITS_DIR__/<scope>.md` (+ siblings consulted for recurrence)
**Proposed:** <today's date>

## Summary

| Metric                         | Value |
| ------------------------------ | ----- |
| Findings reviewed              | N     |
| Proposals (clustered)          | N     |
| Advancing via /fw-spec         | N     |
| Advancing via /fw-plan         | N     |
| Accepted (no fix)              | N     |

## Proposals (ranked by leverage)

### P1 -- <title>  [disposition]

**Problem**
- <finding, with verbatim evidence pointer from the retro>
- Recurrence: <appears in scopes X, Y / single-scope>

**Proposed outcome**
- <observable change -- not an implementation>

**Handoff:** /fw-spec <name> | /fw-plan <description> | accept -- do not fix
**Leverage:** <recurrence x blast radius x cost-of-inaction vs fix-cost, one line>
**Operator decision:** <advance / defer / accept -- filled from STEP 6>

## Not proposed (findings reviewed, no action)

- <finding> -- <why no action: one-off / inherent / below the line>
```

## STEP 8: PRESENT AND HAND OFF

```
## Proposals ready

Wrote `__FW_PROPOSALS_DIR__/<scope>.md`.

**Headline:** <one sentence>

**Advancing:**
- P1 <title> -> run /fw-spec <name>
- P3 <title> -> run /fw-plan <description>

**Accepted (no fix):**
- P2 <title> -- <one-line reason>

Run the handoff command for each advancing proposal when ready.
```

## RULES

1. **Every proposal traces to a cited retro finding.** The retro owns the evidence; this command reuses its pointers verbatim.
2. **Propose outcomes, not implementations.**
3. **Cluster, do not enumerate.** Symptoms of one root cause collapse into one proposal.
4. **Rank by leverage; recurrence beats one-offs.**
5. **"Accept -- do not fix" is a real proposal.** Inherent costs get named and dismissed with a reason.
6. **Do not invent work to fill the doc.** Few or zero proposals is a correct result.
7. **The operator owns prioritization.** Use AskUserQuestion; advance only what they pick.
8. **No emojis.**
