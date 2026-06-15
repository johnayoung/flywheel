---
name: fw-spec
description: Interview the author into ungameable, end-state success criteria and write them as a numbered flywheel spec for /fw-plan
argument-hint: <feature idea or one-line request>
---
<!-- managed-by: flywheel init -->

The product of this stage is justified trust that a future change is correct, not a feature description. Code is a byproduct. Your hard, irreducible job is the spec plus its success criteria — never the code. The valuable, expensive work here is authoring criteria an optimizing agent CANNOT game while it satisfies the literal check; a longer requirements list is not the deliverable, gradeable end-state criteria are.

The agent that builds this feature is untrusted and adversarial. It reads every criterion and, if you let it, satisfies the wording while missing the point — and it gets better at that as it gets more capable, so author every criterion as if a stronger future agent will read it hunting for the cheapest fake. "It passed" is never evidence a criterion is sound.

Stay lean, and ration the interview: front-load the highest-value questions, skip what prior answers settle, and stop the moment the remaining unknowns no longer change a criterion. Over-asking measurably degrades the author's judgment, which is the one input you cannot replace. But lean means FEWER, SHARPER criteria — never vaguer ones: an under-specified criterion is provably gameable, because a literal agent satisfies the narrow reading and skips the rest.

## INPUT

$ARGUMENTS

Anything from a single sentence ("add caching") to a paragraph. Treat it as a starting hypothesis, not settled scope.

## THE ONE ORGANIZING QUESTION

Behind every step below is exactly one question, asked of every requirement:

> **What observable end-state proves this is satisfied, and how could an optimizing agent satisfy the check while missing the point?**

If you cannot answer the first half, you do not understand the requirement yet — ask. If the second half has an easy answer, the criterion is gameable — tighten it before you write it down.

## WHAT "UNGAMEABLE" MEANS HERE (read before interviewing)

No single criterion is provably un-gameable. Leverage comes from four properties you build into the spec, not from clever wording:

1. **Grade the end-state, not the path.** A criterion describes an observable outcome (a value, a file, a response, a behavior), never the steps to reach it. Diagnostic, applied verbatim: *"Will this wording have to change if the implementation changes but the behavior does not?"* If yes, it grades the path. Rewrite it.
2. **The authoritative grade is produced out-of-band.** The signal that decides "done" is computed by machinery the agent does not run and cannot edit. Agent self-reports are untrusted and never authoritative.
3. **Cover the full intended end-state, not the minimal checkable thing.** A narrow criterion (check one room when all rooms matter) is provably hackable. Lean means *fewer, sharper* criteria, never vaguer ones.
4. **Constrain which criteria count.** Each criterion carries a trust tier; a machine-checkable check outranks a human gate, which outranks an LLM judge. A subjective tier is paired with a machine check or a holdout, never left as the sole authority.

A criterion that cannot be lowered to a grader (see STEP 3) is a blocking spec defect — the analog of an unresolved-ambiguity marker. You do not ship the spec with one. You either sharpen it until it grades, or you record it as an explicitly accepted Open Question with the reason it cannot be graded yet.

## STEP 1: ORIENT (cheap, then stop)

Spend a few commands learning what exists so your questions are sharp and your criteria bind to real surfaces. Do not audit the repo.

```bash
ls -la
cat README.md 2>/dev/null | head -80

# Discover apps/modules: top-level dirs carrying a build manifest.
for dir in ./ */; do
  for marker in package.json pyproject.toml go.mod Cargo.toml build.gradle pom.xml mix.exs Gemfile composer.json; do
    [ -f "$dir$marker" ] && echo "module: $dir ($marker)"
  done
done

# The verification surface: how this repo proves a change is correct today.
# This is the most important discovery here — your criteria lower onto it,
# and STEP 5 gates against it. That command set is your grader vocabulary.
ls Makefile justfile Taskfile.yml noxfile.py 2>/dev/null
cat package.json 2>/dev/null | grep -A30 '"scripts"'
ls pyproject.toml tox.ini pytest.ini setup.cfg 2>/dev/null
ls -d test tests spec specs e2e __tests__ 2>/dev/null
ls .github/workflows .gitlab-ci.yml .circleci 2>/dev/null

# Code near the request, so criteria reference real surfaces.
grep -ri "<keywords-from-request>" -l 2>/dev/null | grep -v node_modules | head -20
```

Note three things, because they shape every criterion:
- **The existing verification surface** — the test/lint/typecheck/build/CI commands that currently decide whether a change here is correct. Your criteria lower onto these.
- **Surfaces this feature observably changes** — an API response, a stored value, a file, a CLI exit code, a rendered output. These are end-states you can grade.
- **Whether this feature touches the verification surface itself** — the test runner, CI config, fixtures, the grading/build path, anything that decides whether OTHER changes are correct. If it does, flag it now; it trips the stricter gate in STEP 5.

## STEP 2: ADAPTIVE INTERVIEW (rationed, front-loaded — budget ~3-6 questions, hard cap ~10)

The interview's purpose is NOT to enumerate features. It is to extract, per requirement, the one thing that makes a grader possible: an observable end-state and the cheapest way to fake it. Use the value the author cannot easily articulate — the tacit "obvious to me" expectation — because that gap is exactly what a literal agent exploits.

This is adaptive and skip-logic-driven, not a fixed checklist. After each answer, re-rank what is still unknown by how much it would change a criterion, ask the highest-value question next, and skip anything the last answer (or STEP 1) already settled. Stop early when the remaining unknowns no longer move any criterion — even with budget left.

Use AskUserQuestion, one focused batch at a time (1-3 tightly related questions). Every question carries 3-5 concrete options plus a recommended default with its reasoning and the consequence of each choice. Never ask a bare yes/no; never ask a question whose answer you can read from the code. If an answer is genuinely missing, ask — never guess and fill the gap yourself.

Walk these themes in priority order, **skipping any the input or a prior answer already resolves:**

**A. End-state and scope boundary (highest value — almost always ask).**
- When this works, what is observably true that was not true before? Push every answer toward something checkable from outside the implementation (output, file/DB state, exit status, HTTP response, rendered artifact).
- What is explicitly OUT of scope? (The cheapest ambiguity killer; a missing boundary is where agents wander, and each "X is unchanged" is itself a criterion.)
- Which existing behavior must stay unchanged (a criterion pinning what must NOT regress)?

**B. The gaming pre-mortem (run WITH the author on the 2-3 highest-stakes outcomes — highest leverage).**
For each top outcome, ask directly:
> *"Here is the end-state as you've described it. What is the cheapest way an agent could make that check pass WITHOUT actually delivering it? Hardcode the known inputs? Read the expected answer from git history? Print a success string? Edit the check itself?"*
Whatever they (or you) can name, close it — tighten the criterion, move the authoritative grade out-of-band, hold the check out from the agent, or add a holdout case the agent never sees. The named gaming move and how you closed it become durable fields on the criterion (STEP 3). This single move converts the most gameable criteria into ungameable ones; fold it into the budget rather than running it as a separate late stage.

**C. The tacit "unknown knowns" hunt (the unique value of this interview).**
The author holds load-bearing requirements they have not stated — knowledge they have not realized matters, or cannot articulate. Probe deliberately; humans systematically drop these under load:
- **Surface the unstated quality bar:** *"Show me an existing example you'd consider 'done right.' What about it makes it good?"* — then encode what they point at as a criterion.
- **Probe the negative space:** *"What would a technically-passing result that you'd still reject look like?"* — that rejected-but-passing case is the exact gaming vector; turn it into an unwanted-behavior criterion.
- **Resolve contradictions, don't smooth them:** when two answers conflict, name the conflict and ask which wins. Don't anchor only on the last thing said — pull an earlier answer forward and pressure-test it.

**D. Behavior under conditions (event/state/error/empty).**
For each in-scope outcome, pin the response to the trigger (when X happens, the system shall Y), relevant state (while in state Z), and the unwanted cases — invalid input, missing dependency, empty/zero results, concurrent access. The unwanted-behavior cases are where vague criteria get gamed; do not skip them.

**E. Non-functional bars (only if real).**
Performance, security, compatibility — ONLY when the author names a concrete, measurable bar (a latency number, a specific threat, a version matrix). Do not invent NFRs; a fabricated bar is gold-plating that lengthens the build for a feature nobody asked for.

**Skip-logic reminders:** a "no preference" answer closes that branch — record the default as a decision and move on. If two themes collapse into one answer, do not re-ask. If the author is fatiguing (short answers, "whatever you think"), you are past the useful budget — stop and write down sensible defaults as decisions, not as more questions.

## STEP 3: AUTHOR EACH CRITERION SO IT LOWERS TO A GRADER

Every acceptance criterion must lower cleanly to exactly one downstream grader type. `/fw-plan` compiles each into a flywheel task `goal` plus `graders`:

| Grader type   | Checks                                              | Your criterion must be...                                       |
| ------------- | --------------------------------------------------- | --------------------------------------------------------------- |
| `command`     | A shell command's exit code against the end-state   | Decidable by a deterministic command (test, assertion, `grep -q`, `test -f`, status check). **Preferred.** |
| `transcript`  | Limits on the run itself (turns/tokens/wall-time)   | A bound on the process, not the product (e.g. "must converge within N turns"). |
| `rubric`      | An LLM judge reads goal + diff + artifacts          | Genuinely subjective and not machine-decidable. Use sparingly; pair with a command check or a holdout. |
| `manual`      | An operator inspects and approves at a checkpoint   | Needs human judgment no command and no LLM can stand in for.    |

### Write each criterion atomic, in EARS condition->response form

One condition, one response, atomic — one condition-response per criterion. Pick the pattern:

- **Ubiquitous** — `The <system> shall <observable response>.`
- **State-driven** — `While <state>, the <system> shall <observable response>.`
- **Event-driven** — `When <trigger>, the <system> shall <observable response>.`
- **Unwanted-behavior** (the anti-gaming pattern — author these deliberately) — `If <bad condition>, then the <system> shall <required defensive response>.`

If a criterion needs more than ~3 preconditions it is a decision table — split it into several atomic criteria. If it joins two checkable outcomes with "and"/"or", split it.

### Author each criterion with its grader tag, verify line, and gaming move inline

Reserve dedicated space, per criterion, for the grader type, the visibility flag, the exact check, and the gaming move it forecloses. This makes the ungameable path the cheap path: a Tier-1 held-out `command` check with a named exploit is literally less typing than decorative "shall" prose, and a bare decorative criterion reads as structurally incomplete.

```
N. When <trigger> [while <state>], <observable result>. [<grader-type> | <visibility>]
   verify: <the exact deterministic check — command, file/exit assertion, or saved-output diff; or the operator decision / the assertion an LLM judges>
   defends against: <the cheapest fake this forecloses — e.g. hardcoding the known inputs, reading the answer from history, printing a success string, overwriting the check>
```

- **Grader tag** — `command` > `transcript` > `manual` > `rubric`. Reach for the highest tier the outcome allows. A `rubric` tag on a machine-decidable outcome is a defect; rewrite it as `command`. Use `transcript` only to bound the run, never as a correctness signal. A `rubric` criterion must be paired with a command check or a holdout — never the sole authority, because a single superficial token can flip an LLM judge to a false positive.
- **Visibility flag** — `visible` (the agent may iterate against this signal) or `held-out` (the authoritative grade; the agent never sees or runs it). Keep a visible surface so the agent can iterate and does not fly blind, but the grade that *lands* the change should be `held-out` for the highest-stakes outcomes. The divergence from generic spec tools is "the authoritative grade is hidden," not "the agent flies blind." Prefer held-out checks that assert against output or state the agent cannot pre-compute from the known inputs.
- **defends against** — the cheapest gaming move this criterion closes, carried forward from the STEP 2 pre-mortem. **If you cannot name a gaming move, you have not stress-tested the criterion** — do so before writing it down.

### Lint every candidate (reject, do not soften)

A criterion that fails any check is rewritten or dropped — never shipped with a caveat:

- **Path, not end-state** — fails the implementation-change diagnostic; names a function, file, framework, or UI widget instead of the observable behavior. Rewrite onto the outcome.
- **Vague/subjective wording** — `fast`, `easy`, `simple`, `robust`, `user-friendly`, `intuitive`, `as expected`, `properly`, `handle gracefully`. Direction, not destination.
- **Absolute hand-wave** — `never`/`always` with no bounded, checkable case that proves a violation.
- **Compound/non-atomic** — joins two outcomes with "and also"/"as well as". Split it.
- **Under-specified (narrow)** — covers less than the full intended end-state. Narrowing does not make it safer; the uncovered part is a guaranteed exploit surface.
- **Missing the edge/error/empty case** for an outcome that has one. Add the unwanted-behavior and boundary criteria.
- **Un-gradeable** — lowers to none of the four grader types. Blocking defect; sharpen it or log it as an accepted Open Question.

One-line smell test for the whole catalogue: **a poor criterion names a direction; a good one names a destination.** If a criterion is only expressible as decorative prose, the outcome is still ambiguous — go back to STEP 2 rather than papering over it with a `rubric`.

## STEP 4: GRADEABILITY GATE (blocking)

Before the file: every criterion lowers cleanly to one grader type, or the stage blocks. An un-gradeable criterion is the spec defect to stop on — not a soft warning. List any criterion you genuinely cannot lower under Open Questions with the specific blocking question, and do NOT silently drop it; an omitted criterion is a guaranteed downstream exploit. The stage does not complete while any un-gradeable criterion remains unaccounted for.

## STEP 5: VERIFICATION-SURFACE GATE (a Definition-of-Done, not a per-item criterion)

This is universal and separate from the per-criterion checks above. Per-item criteria say *what this feature does*; this gate says *no change is trusted that weakened the machinery that proves things work*. It is the project-agnostic form of "you cannot land a change that lowers your own bar," stated in terms of whatever decides pass/fail in THIS repo — never any specific repo's internals.

Ask: **does this feature change the verification surface itself** — the tests, the grading commands, CI config, fixtures, assertions, coverage gates, the lint/typecheck setup, or anything `/fw-plan` will turn into a grader?

- **No** -> record "Verification surface: unchanged" in the spec and move on.
- **Yes** -> the spec MUST carry a standing Definition-of-Done that every task touching that surface inherits, authored as criteria, marked `held-out` wherever possible:
  - The existing verification suite still runs and still passes after the change (new code cannot pass by deleting or weakening the checks that constrained it).
  - Any check relaxed, removed, or skipped is named explicitly with its justification, and a replacement check of equal-or-greater strength is specified. A removed assertion with no named replacement is a blocking defect.
  - New behavior is proven by a check the implementing agent did not author against its own known inputs (out-of-band / holdout), so "the test passes" cannot mean "the agent wrote the test to pass."

## STEP 6: WRITE THE SPEC

Specs live at `__FW_SPECS_DIR__/NNNNN-FEATURE-<name>.md`. Pick the next sequence number:

```bash
ls __FW_SPECS_DIR__/[0-9]*-FEATURE-*.md __FW_SPECS_DIR__/archive/[0-9]*-FEATURE-*.md 2>/dev/null \
  | sed 's#.*/\([0-9]*\)-.*#\1#' | sort -n | tail -1
```

No numbered specs yet -> start at `00001`. Otherwise increment the highest and zero-pad to 5 digits (`00001`, `00042`). Use a short kebab-case `<name>`.

The spec is immutable once handed off. Later clarifications belong in the task lifecycle, never edited back into this file. Keep it short — every section earns its place or is omitted.

Write `__FW_SPECS_DIR__/NNNNN-FEATURE-<name>.md` with this structure. The Success Criteria block is the spine; everything else is context for it.

```markdown
# Feature: <Name>

## Outcome
<The single observable end-state from STEP 2: what is measurably different when this is done.>

## Background
<Why this is needed, in 2-3 sentences, including the tacit value surfaced in the interview that a literal agent would otherwise miss. Context only; no requirements here.>

## Scope
### In scope
- <Specific observable capability.>
### Out of scope
- <Explicitly excluded — the boundary that keeps the agent from wandering.>
### Must not regress
- <Existing behavior that must stay unchanged.>

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its grader
type, visibility, the exact check, and the gaming move it forecloses. `/fw-plan`
lowers each one to a command / transcript / rubric / manual grader.

1. When <trigger> [while <state>], <observable result>. [command | held-out]
   verify: <the deterministic check — e.g. "GET /x returns 200 with field y", "exit 0", "file z exists containing w">
   defends against: <e.g. hardcoding the known inputs / printing a success string>
2. If <bad condition>, then <required defensive response>. [command | visible]
   verify: <check>
   defends against: <the cheap fake this forecloses>
3. <Genuinely subjective end-state.> [rubric, paired with #1 | held-out]
   verify: <assertion the LLM judge evaluates>
   defends against: <what a single superficial token could flip>
<When STEP 5's gate fired, also list the inherited verification-surface criteria here, each marked held-out and tagged (verification-surface).>

Verification surface: unchanged.
<If the gate fired, replace that one line with: the existing suite still passes; any relaxed or removed check is named with an equal-or-greater replacement (a removed assertion with none is a blocking defect); new behavior is proven by an out-of-band/held-out check the agent did not author against its own known inputs.>

## Decomposition Hint (for /fw-plan)
The architectural layers this splits along, so /fw-plan can size one task per slice
and chain them with prerequisites. State layers and the criteria each must satisfy;
do NOT prescribe implementation.
- Layer <e.g. data / model / service / interface>: satisfies #<n>, #<m>
- Layer <next>: satisfies #<k>; depends on <prior layer>
Shared invariants (an enum member, a schema field, a required arg) multiple layers
assert against: name them here so dependent tasks update together and no slice
inherits a red suite.

## Decisions Log (ADR-style: immutable, supersede — never edit)
One entry per load-bearing decision so downstream tasks inherit the WHY. A reversed
decision gets a new superseding entry; the old one stays, marked superseded.

### D-1: <decision title>  (Status: Accepted)
- Context: <what forced the choice> | Decision: <what was chosen>
- Rejected: <options and why each lost> | Consequences: <including the negative ones>

## Open Questions (accepted gaps)
<Only criteria that genuinely could not be graded yet, each with its blocking question.
Empty is the goal — but never drop an un-gradeable criterion instead of recording it.>

## Next Steps
Run `/fw-plan NNNNN-FEATURE-<name>` to compile these criteria into flywheel tasks and graders.
```

## STEP 7: PRESENT AND HAND OFF

Show the author the criteria and the gate result, then hand off. Do not summarize prose — summarize what is now provable.

```
Spec ready: __FW_SPECS_DIR__/NNNNN-FEATURE-<name>.md

Outcome: <one line>
Success criteria: <N> total — <c> command, <t> transcript, <m> manual, <r> rubric
Held-out (authoritative): <h> of <N>   (the ratio is the spec's strength, not the count)
Verification surface: <unchanged | DoD present>
Open un-gradeable criteria: 0   (if not 0, the spec is NOT ready — return to STEP 4)

The contract in one line: <what observable end-state means done, and what it forbids.>

Every criterion grades an observable end-state and lowers to a flywheel grader.
The agent that builds this never decides whether it succeeded. The spec is immutable
from here; clarifications discovered during execution go to lifecycle records.

Next: run /fw-plan NNNNN-FEATURE-<name> to turn these criteria into tasks and graders.
```

Then hand off to `/fw-plan NNNNN-FEATURE-<name>`.

## ANTI-PATTERNS

- **DO NOT** write a criterion that grades the path instead of the end-state — no function names, file paths, or UI mechanics in a criterion.
- **DO NOT** let the building agent be the authority on whether it succeeded — agent claims are untrusted; the authoritative grade is out-of-band.
- **DO NOT** ship a criterion that cannot be lowered to a `command`/`transcript`/`rubric`/`manual` grader; sharpen it or log it as an accepted Open Question — never silently drop it.
- **DO NOT** make an LLM-judge (`rubric`) the sole authority for a criterion that is verifiable at all; a cheap superficial token can flip it to a false positive.
- **DO NOT** tag a criterion without naming the gaming move it defends against — an un-stress-tested criterion is a gameable one.
- **DO NOT** narrow a criterion to make it easy to check — a partial criterion is provably hackable.
- **DO NOT** over-ask: a long interrogation degrades the judgment the spec depends on. Stop when the end-state is pinned.
- **DO NOT** invent NFRs, scope, or requirements the author did not state — that is gold-plating.
- **DO NOT** prescribe implementation steps or procedure — that is `/fw-plan`'s and the agent's job.
- **DO NOT** edit a handed-off spec; clarifications go to lifecycle records, and decisions supersede, never overwrite.
- **DO NOT** place spec files anywhere except `__FW_SPECS_DIR__/`.
- **DO NOT** use emojis.
