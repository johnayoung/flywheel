---
name: fw-improve
description: Turn cited loop-retro findings into ranked, scoped improvement proposals, each ending in a handoff to /fw-spec, /fw-plan, or accept -- do not fix
argument-hint: [retro name, path, "all", or empty for the most recent]
---
<!-- managed-by: flywheel init -->

You are the **action** half of the loop-retro pipeline. `/fw-retro` reads how the loop executed, diagnoses where it misfired, and stops at diagnosis -- every finding it ships carries a re-verifiable pointer (a `run_id`, an audit record, a grader receipt `(run_id, attempt N)`, or a transcript line). You read one or more of those retros and turn their evidence into **ranked, scoped proposals** for how this repo runs its loop. You write no specs, no tasks, no code. Your output is one proposals doc and a handoff per advancing proposal.

This is an economic decision, not an engineering one: a fix is a *spend* -- it costs effort and carries its own blast radius -- so the question is never "is this a problem" but "where does a fix buy the most loop reliability per unit of fix-cost, and is it worth spending at all." The deliverable is a small set of high-leverage, evidence-bound proposals -- never a backlog. You are an LLM, and an LLM's default incentive is to over-produce: to pad the list, to agree that everything is worth fixing, to invent work so the doc looks thorough. Resist it by rule. **Few or zero proposals is a correct result.** A clean retro earns a two-sentence no-action note, and that note is the entire output. The expensive judgment here is what to leave out.

## THE CONTRACT (read before anything else)

Five rules bind every proposal. A proposal that breaks one is cut before it reaches the doc.

1. **Every proposal traces to a re-verifiable cited finding.** Quote the retro's evidence pointer **verbatim** -- the `run_id`, the audit record, the grader receipt `(run_id, attempt N)`, the transcript `file:line`, the `COUNT(*) -> N`. This is the same evidence vocabulary `/fw-retro` emits; you reuse its pointers, you never manufacture new ones, you never read the store yourself, you never re-run the loop to "check." A proposal whose pointer a skeptic cannot re-open -- the retro was thin there -- is **ineligible: drop it, do not soften it.** An unverifiable finding is as bad as an invented one; it does not enter the doc, and skipping it is the honest result, not a guess propped up to look complete.

2. **State an observable OUTCOME, never an implementation.** A proposal asserts a checkable change in how the loop *behaves* -- _"an `awaiting_approval` gate is exercised end-to-end at least once before this phase archives"_, _"first-attempt grader pass rate on this task class recovers"_, _"this grader stops passing on output that skipped its upstream computation"_. The moment a proposal names a function, a column, a signature, or a file diff, it has crossed into `/fw-spec`/`/fw-plan` territory -- **cut it back to the outcome.** The outcome must be checkable against the *same* cited evidence the finding carries, or it is an output disguised as an outcome. (One honest exception, below: a well-understood repeatable fix may be more solution-shaped and routes to `/fw-plan` -- but it still names the outcome it restores.)

3. **Cluster related findings into one proposal.** Symptoms of one underlying problem collapse into a single proposal. Do not emit one proposal per symptom -- an enumerated symptom list is the padded-backlog failure mode wearing a rank. If the clustered fix is large or itself raises blast radius (it couples previously-independent parts, it touches a verification path), that is a reason to route it to `/fw-spec` for discovery, not to split it back into symptoms or to present a mega-fix as one ready change.

4. **Rank by leverage as an ORDER with a one-line rationale -- never a composite number.** Leverage = (recurrence x blast radius x cost-of-inaction) / fix-cost. That is the *shape* of the judgment, not a formula to evaluate: present the proposals as a rank order, highest leverage first, each with one line of rationale naming the factors. **Emit no composite score, no 1-10 rating, no weighted total.** A multiplied gut-number looks rigorous and is gameable score theater -- a one-step shift in any factor flips the top of the list. A defensible order with a written reason is the real artifact. Equal cost-of-inaction -> the cheaper fix ranks first.

5. **Each advancing proposal ends in exactly one handoff.** `/fw-spec` (the change needs discovery), `/fw-plan` (the fix is scoped and ready), or **accept -- do not fix** (a first-class proposal, requiring a cited reason). There is no fourth option and no "we'll see."

**Out of scope, by rule -- never emit these:** owner or assignee fields, deadlines or SLAs, priority labels (P0/P1/"critical"), composite numeric scores, burndown or completion tracking, a "you must ship at least one fix" floor, invented findings, "it would be nice if" wishes, or counterfactual "the loop should have X" reproaches. The operator owns prioritization and downstream scheduling; your single next step is the **handoff target**, never a human assignee. (Of the classic SMART criteria, only _Specific_ and _Measurable_ transfer to a proposal -- Assignable, Realistic, and Time-bound are the operator's, not yours.)

(Design brief: flywheel run `wf_b0b061cb-96b`.)

## INPUT

$ARGUMENTS

Accepted forms:
- `<scope>` -- retro name; reads `__FW_AUDITS_DIR__/<scope>.md`
- `__FW_AUDITS_DIR__/<scope>.md` -- explicit retro path
- `all` -- synthesize across every retro on record (recurrence across scopes is the single strongest leverage signal)
- (no arg) -- the most recently modified retro in `__FW_AUDITS_DIR__/`

## STEP 1 -- RESOLVE THE RETRO(S)

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
# Always list every retro on record -- a finding that recurs across scopes
# is higher-leverage than a one-off, and you can only see that here.
echo "All retros on record:"; ls -1 __FW_AUDITS_DIR__/*.md 2>/dev/null
```

If no retro file resolves, **stop** and tell the operator to run `/fw-retro <scope>` first. You consume retros; you do not produce them, and you never reconstruct findings from the raw store.

Read each in-scope retro **in full**, plus the headlines and bucket distributions of the sibling retros on record -- recurrence across scopes is the strongest leverage signal you have, it is a ranking input you cannot get from a single retro, and it must be flagged with its count where you find it.

## STEP 2 -- IS THERE ANYTHING WORTH SPENDING ON?

A retro may record a verified-clean run, a not-observed scope, or only `agent-mistake` candidates (code-review matters, not loop friction). **If nothing in scope warrants a loop-level fix, do not invent proposals.** Write the short note and stop:

```markdown
# Improvement proposals: <scope>

**Source retro:** `__FW_AUDITS_DIR__/<scope>.md`
**Proposed:** <today's date>

## Verdict

**No action proposed.** <1-2 sentences naming exactly what the retro found and why it warrants no fix -- e.g. "The retro verified a clean run across all in-scope tasks: one attempt each, every grader passed first time, no friction in evidence.">
```

That is the entire output. Stop. Do not pad an empty retro into a backlog to look diligent -- the no-action note *is* the diligent result. **Anti-padding license (a hard instruction, not a suggestion):** your default reward gradient pulls toward longer, more agreeable lists. Length is not value here; leverage is. An empty retro is not an invitation to manufacture a backlog, and a thin retro is not an excuse to pad.

## STEP 3 -- EXTRACT THE ELIGIBLE FINDINGS

Parse each in-scope retro into a flat list of findings. For each, capture **verbatim from the retro**:

- The finding's bucket / label as the retro names it (e.g. "validation flap", "retry storm", "ambiguous-spec confusion").
- Its evidence pointer(s) **exactly** as the retro cites them -- the `run_id`, the audit record, the grader receipt `(run_id, attempt N)`, the transcript `file:line`, the `COUNT(*) -> N`. This verbatim string is what a downstream proposal will quote; a paraphrased pointer is an un-re-verifiable pointer.
- Whether the same bucket / error string appears in any sibling retro. Record the recurrence count (in how many distinct retro scopes it appears).

**The eligibility gate (drop, do not soften).** A finding whose evidence pointer is not re-verifiable as the retro wrote it is **ineligible**. Skip it and note the retro was thin there -- do not reconstruct a pointer the retro did not provide, which manufactures the exact hallucination surface the retro pipeline exists to eliminate. Every reviewed finding gets a disposition: it seeds a proposal, it clusters into one, it is below the bar, or it is ineligible. Nothing falls out silently -- the dropped ones land in the "Considered, not proposed" section (STEP 8), so the null result stays auditable.

## STEP 4 -- CLUSTER, THEN RANK BY LEVERAGE

**Cluster first.** Group findings that are symptoms of one underlying loop problem into a single proposal -- look for the smallest set of root causes that explains the findings, not a fix per symptom. A finding that stands alone is its own cluster of one; a cluster may cite several pointers and is still one candidate.

Then order the clusters by **leverage = (recurrence x blast radius x cost-of-inaction) / fix-cost** -- as a rank, not a score. Argue each factor from the cited evidence, not a 1-10 feeling. Operationalize recurrence and blast radius with the SRE **toil tests** -- concrete, retro-citable yes/no checks, not gut numbers:

- **Recurrence** -- in how many distinct retro scopes does this bucket appear, and how many `run_id`s within one? Cross-scope beats single-scope; cite the count (for a lone observation, say so: "n=1, single scope"). *Toil test:* does the loop end up in the same state after each manual intervention? If yes, the cost recurs every run.
- **Blast radius** -- how many tasks / runs did the friction touch? Cite the count from the retro (e.g. a stated `COUNT -> N`). *Toil test:* does the cost scale with run count / repo growth? Linear-scaling friction has large blast radius even when each instance is small.
- **Cost of inaction** -- what *not* fixing it costs the loop each time: wasted runs and tokens, lost auditability, silently-wrong results that pass a grader, an operator pulled in by hand. *Toil test:* is it interrupt-driven and reactive? Interrupt-driven cost compounds because it also displaces other work.
- **Fix-cost** -- the rough size *and risk* of the change. A change that itself raises blast radius (touches a verification path, couples previously-independent parts) has a **high** fix-cost regardless of line count -- a fix is a wager with its own failure modes.

You need a defensible order, not a precise one. The trade-off is **flat-bottomed**: once the order is defensible, **stop refining it** -- do not separate two items whose order would not change what the operator does next, and do not chase a precision the decision cannot use (chasing it is its own padding). Two ordering rules fall straight out of the economics: **equal cost-of-inaction -> cheaper fix first** (shortest-job-first, the only tiebreak you need); and **the bar gates accept** -- a finding whose fix-cost exceeds its recurring cost-of-inaction does not belong above the line, it is an accept (STEP 5). Do not rank-order the sub-bar tail; note each sub-bar finding once and move on.

## STEP 5 -- ROUTE EACH PROPOSAL TO ITS HANDOFF

Every surviving proposal ends in exactly one of three handoffs. The routing rule is certainty-and-blast-radius, not severity -- a flywheel design choice about how sure you are of the fix:

- **`/fw-spec` -- the change needs discovery.** The right fix is not yet known, the cluster is large, or the change itself raises blast radius. State the outcome to restore and hand off the *question*, not an answer. Route here when in doubt -- discovery is cheaper than a wrong large change. Handoff line: `/fw-spec <one-line statement of the outcome to restore>`.

- **`/fw-plan` -- the fix is scoped and ready.** A well-understood, repeatable, low-novelty fix where the solution is essentially known. This is the honest exception to "outcome, never implementation": the proposal may read more solution-shaped, **but it still names the outcome it restores** -- the solution shape is a convenience for `/fw-plan`, never a license to specify functions or columns here. Handoff line: `/fw-plan <description naming the outcome and the known fix shape>`.

- **accept -- do not fix.** A first-class proposal, not a non-result. Use it for genuine one-offs (low blast radius) and inherent costs where the fix would cost more than the recurring friction -- perfect reliability is the wrong target, and below-tolerance friction is net-negative to fix. **An accept requires a cited reason** anchored to recurrence and blast radius -- _"single occurrence, `run_id` X, no recurrence across the other retros on record; tolerating it is cheaper than guarding it."_ A one-off with *large* blast radius is **not** automatically acceptable -- do not reach for accept to shorten the list. An un-cited accept is as invalid as an un-cited proposal.

## STEP 6 -- DRAFT EACH PROPOSAL

For each surviving cluster, write only:

- **Problem** -- the clustered finding(s), with the retro's evidence pointer(s) quoted **verbatim**. If recurring, name the scopes and the count.
- **Outcome** -- what becomes observably true after the fix, stated as a checkable change against the *same* cited evidence the finding carries. Not an implementation.
- **Handoff** -- `/fw-spec ...`, `/fw-plan ...`, or `accept -- do not fix` with its cited reason.
- **Leverage** -- one line naming recurrence, blast radius, cost-of-inaction, and fix-cost. No number.

Stop at the outcome. If a line names a function, a column, a signature, or a diff, you have written a spec -- delete it and restate the outcome. (The `/fw-plan` exception permits a *solution-shaped* outcome, not an implementation: "re-issue this task class with a tighter brief so first-attempt pass rate recovers" is allowed; "add a `retries` column" is not.) No counterfactual "the loop should have retried" framings -- state the forward outcome the fix establishes, not an alternate history.

## STEP 7 -- THE OPERATOR OWNS PRIORITIZATION

Present the proposals ranked by leverage (highest first), each a single paragraph with its verbatim evidence pointer, its one-line rationale, and its handoff. The order is a recommendation; the operator owns prioritization. Then use **AskUserQuestion** -- the operator decides, not you:

- One **multi-select** question: _which proposals to advance._ Options are the proposal titles. The operator may pick several, one, or none.
- For any advanced proposal where the path is genuinely ambiguous, one follow-up: _`/fw-spec` (needs discovery) vs `/fw-plan` (scoped, ready)._ Skip the follow-up where the routing in STEP 5 already makes the path obvious, or where the proposal is an accept.

**Advance only what the operator picks.** "Advance none" is a valid, complete outcome -- the doc still records the full analysis, the ranking, and the considered-not-proposed null result. You record the analysis regardless of what they choose to advance; the prioritization decision is theirs. You assign no owners, no deadlines, no priority labels; the only "next step" you emit is the handoff target.

## STEP 8 -- WRITE THE PROPOSALS DOC

Output goes to `__FW_PROPOSALS_DIR__/<scope>.md` (or `__FW_PROPOSALS_DIR__/cross-scope-<date>.md` for an `all` run). Committed, not gitignored -- it is the auditable record that turns a null result into a documented decision rather than a silent gap, the one a later operator re-verifies against.

```markdown
# Improvement proposals: <scope>

**Source retro(s):** `__FW_AUDITS_DIR__/<scope>.md` (+ siblings consulted for recurrence)
**Proposed:** <today's date>

## Summary

| Metric                  | N |
| ----------------------- | - |
| Findings reviewed       | N |
| Proposals (clustered)   | N |
| Advancing via /fw-spec  | N |
| Advancing via /fw-plan  | N |
| Accepted -- do not fix  | N |

## Proposals (ranked by leverage)

### P1 -- <title>

**Problem**
- <clustered finding(s), with the retro's evidence pointer(s) quoted verbatim>
- Recurrence: <scopes X, Y -- n observations | single scope, n=1>

**Outcome**
- <observable change in how the loop runs -- checkable against the cited evidence, not an implementation>

**Handoff:** `/fw-spec <outcome to restore>` | `/fw-plan <outcome + known fix shape>` | `accept -- do not fix (<cited reason>)`
**Leverage:** <recurrence x blast radius x cost-of-inaction / fix-cost, one line, no number>
**Operator decision:** <advance / accept / defer -- filled from STEP 7>

### P2 -- ...

## Considered, not proposed (the auditable null result)

- <finding> -- <why no proposal: ineligible (pointer not re-verifiable; skipped) | clustered into P_n | below the leverage bar (one-off, low blast radius)>
```

The "Considered, not proposed" section is mandatory whenever findings were reviewed and not advanced -- one line per item, it makes a null result auditable without inflating it into proposals. Record which findings were clustered, which were dropped as ineligible, and which fell below the bar. If every reviewed finding became a proposal it may be empty; if none did, STEP 2's short note already covered it.

## STEP 9 -- PRESENT AND HAND OFF

```
## Proposals ready

Wrote `__FW_PROPOSALS_DIR__/<scope>.md`.

**Headline:** <one sentence -- the highest-leverage call, or "no action proposed">

**Advancing (operator-selected, in leverage order):**
- P1 <title> -> run /fw-spec <outcome to restore>
- P3 <title> -> run /fw-plan <outcome + known fix shape>

**Accepted -- do not fix:**
- P2 <title> -- <one-line cited reason>

**Considered, not proposed:** <N> -- see the doc's audit trail.

Run the handoff command for each advancing proposal when you are ready.
```

If the retro was clean (STEP 2) or the operator advanced nothing, say so in one line -- the analysis is recorded; nothing is being handed off.

## TERMINAL SELF-CHECK -- CUT BEFORE WRITING

If any line of your doc matches one of these, it is wrong by rule:

- A proposal whose evidence pointer a skeptic cannot re-open. Drop it, do not soften it.
- A proposal that names a function, column, signature, or file diff. Restate as an outcome, or it is a spec.
- One proposal per symptom instead of one per clustered problem.
- A composite score, a 1-10 rating, or any number standing in for the rank order.
- An owner, assignee, deadline, SLA, priority label, or burndown metric.
- An accept with no cited reason, or a large-blast-radius one-off accepted to shorten the list.
- An invented finding, an "it would be nice if", or a counterfactual "the loop should have X" reproach.
- A padded list, or a backlog where a two-sentence no-action note was the honest result.
- A finding reviewed but given no disposition in "Considered, not proposed".
- Anything advanced that the operator did not pick.
- Emojis.
