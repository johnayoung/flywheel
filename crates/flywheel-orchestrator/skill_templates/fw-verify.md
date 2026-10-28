---
name: fw-verify
description: Blind-author the discriminating held-out test ORACLE for fw-plan's tasks before execute, so the agent being graded never writes the test that grades it
argument-hint: '[NNNNN-FEATURE-name, a phase dir, one or more task ids, or empty for the latest plan]'
---
<!-- managed-by: flywheel init -->

The product of this stage is a held-out test the implementing agent did not author and cannot have shaped: an oracle written BLIND to the implementation and proven to DISCRIMINATE, so "the test passes" is evidence the behavior is right rather than evidence the agent wrote a test it could pass. This stage runs BETWEEN `/fw-plan` and execute (`flywheel worker`) and is optional but recommended. It lands NOTHING runnable into the committed repo: the oracle is authored and run at verify time in a git-ignored scratch location (`.flywheel/verification/`), and the only durable artifact it keeps is the recorded DISCRIMINATION PROOF under `__FW_AUDITS_DIR__`. The criterion's permanent regression guard is the owning task's own tests in the repo's normal suite (run by `/fw-plan`'s command graders and CI), never this throwaway oracle — so the held-out file never persists in the tree, never reaches the implementing agent's worktree, and never erodes the next spec's blindness.

The single causal lever is ACCESS, not separation: an oracle derived from code-in-view encodes the implementation's actual (often buggy) behavior as the assertion. So the spine of this skill is an INFORMATION FENCE: an exact may-see / must-not-see contract, packaged as a self-contained FENCE PACK handed to a blind author who has never seen the implementation, the agent's visible tests, any runtime output, or a reference solution. Every other step exists to keep that author blind, pinned to the DECLARED contract, and producing an oracle that DISCRIMINATES rather than merely exists.

The durable artifact you keep is the DISCRIMINATION PROOF: for each admitted holdout, which synthesized plausible-wrong reference died, on which input, and that a synthesized correct reference passed. That recorded evidence is the only thing that distinguishes an admitted holdout from a green test that grades nothing. Agent claims are untrusted everywhere in flywheel; here you make the same demand of the oracle itself, kill-and-pass evidence, not the test's mere existence or greenness.

You author in two roles, and you must keep them apart. YOU (this skill's running agent) orchestrate: you read the planned tasks, triage them, assemble the fence pack, run the discrimination gate, fence the result, and report. The BLIND AUTHOR is a fresh context you fan out via the Task tool, given ONLY the fence pack. Never let what you have read (the spec, the plan, an implementation you imagined) leak into that pack.

## THE INFORMATION FENCE (read before anything else, this is the spine)

A blind author is handed exactly one criterion and an observable contract and asked to write a test that would fail a plausible-wrong implementation of it. The fence is the list of what that author MAY and MUST NOT see. A violation in EITHER direction voids the oracle.

**The author MAY see (the whole legitimate surface, assemble all of it):**
- **The one held-out-flagged criterion** with its exact `verify:` line and its `defends against:` line from the spec. One criterion per author; never the whole spec.
- **The observable contract / interface**: signatures, input/output shapes, side-effects, declared pre/post-conditions and invariants (the Design-by-Contract vocabulary), and any concrete examples the spec PINS. This is the black-box surface: intended behavior and the observable interface, never internal structure.
- **The repo's test conventions**: framework, fixtures, assertion style, naming, the directory layout, and one existing wiring example, so the test is executable and idiomatic here. Learn these from existing COMMITTED tests, never from any implementation of the task you are grading.

**The author MUST NOT see (each one re-teaches the wrong oracle):**
- **The implementation under test** (it does not exist yet, the author runs before execute). Never describe, sketch, or pseudocode it. Code-in-view turns the oracle into a transcript of buggy behavior.
- **The agent's own visible tests.** The oracle must be independent of what the graded agent writes.
- **Any runtime, compiler, or candidate output.** Shown an actual output, an author aligns the assertion to the observed value and certifies the bug. Never iterate the oracle against a produced result (see TIMING).
- **A reference solution or the "canonical algorithm"**, both the real one and the one the author remembers from pre-training. The author writes from THIS criterion's declared contract, not a memorized standard.

**The fence cuts both ways, blindness alone is NOT enough.** A blind author handed a thin criterion hallucinates a canonical spec and tests behavior nobody asked for. So the criterion you hand over must be explicit and project-specific. If it is not, if the author cannot pin a discriminating example or relation from the criterion plus contract alone, that is a fence violation in the OTHER direction, and it routes UPSTREAM (see CONTRACT-PINNING FEEDBACK), never into a fabricated assertion. The two failure modes are symmetric: a vague criterion with no discriminator AND a hidden oracle demanding behavior the contract never stated are BOTH defects, fixed upstream, never papered over here.

**The honest gap (state this plainly; do not oversell).** This stage proves blind that a discriminating oracle EXISTS for a criterion and records that proof; it does not by itself grade the agent's real run. The oracle stays in a git-ignored scratch dir, so it never lands and never reaches the agent's worktree — but the execute-time gate on the agent's actual work is therefore the task's OWN command graders and tests, not this oracle. An execute-time HELD-OUT gate — the agent graded by a test it never authored or saw — additionally needs out-of-worktree grading owned by the orchestrator (the agent finishes, then a copy of the oracle grades its committed result from outside the sandbox); that is complementary orchestrator work, explicitly OUT OF SCOPE here. fw-verify must never claim authoritativeness it cannot enforce at authoring time. A held-out suite is also a filter, not a correctness proof: a finite oracle can still be slipped past.

## INPUT

$ARGUMENTS

Accepted forms:
- A spec reference `NNNNN-FEATURE-<name>` -> read its criteria and their held-out flags from `__FW_SPECS_DIR__/NNNNN-FEATURE-<name>.md`, and the tasks `/fw-plan` compiled from it under `__FW_TASKS_DIR__/active/`.
- A phase directory name or one or more task ids -> read those planned task JSONs under `__FW_TASKS_DIR__/active/`, matched back to the spec each was compiled from in `__FW_SPECS_DIR__/`.
- (empty) -> the most recently planned phase under `__FW_TASKS_DIR__/active/`, matched back to its spec in `__FW_SPECS_DIR__/`. Never author for everything by reflex; confirm scope before fanning out.

You READ specs and tasks; you never edit them. The spec is immutable, and the task definition `/fw-plan` produced is immutable. You WRITE the held-out oracle only into the git-ignored verification scratch location (named in STEP 5), RECORD its discrimination proof under `__FW_AUDITS_DIR__`, WRITE the admitted oracle's `<held-out-root>/<task_id>.json` registration to the configured out-of-worktree held-out root when `[held_out] root` is set (STEP 5), and you PRESENT the one task-side edit (a `non_goals` fence) for the operator to apply. Clarifications you discover route upstream (CONTRACT-PINNING FEEDBACK), never into the spec or the task JSON.

## WHAT THIS STAGE IS AND IS NOT

- **It IS** the blind-author step for held-out BEHAVIOR criteria with an observable contract. It reads the tasks `/fw-plan` produced and the spec criteria they compile, authors the oracle blind, validates that it discriminates, screens it for flakiness, runs it in the git-ignored verification scratch dir, records the proof, and presents the fence to add to the owning task's `non_goals`. It supplies the blind discrimination EVIDENCE behind `/fw-plan`'s grader-ladder rung-3 for the criteria that warrant one; the execute-time grader on the agent's real work stays `/fw-plan`'s (the task's own command graders and tests), not a landed copy of this throwaway oracle.
- **It is NOT** a re-plan or a re-spec. You do not invent criteria, add requirements the contract does not state, edit a handed-off task, or rewrite a grader. `/fw-plan` owns the task and grader shape.
- **It is NOT** authoritative-by-itself. The held-out flag becomes a kept promise only when this blind, discriminating authoring is later paired with run-time grader isolation (out of scope, above).

## STEP 1: TRIAGE FIRST, ROUTE EVERY HELD-OUT CRITERION BEFORE WRITING A SINGLE TEST

Read the planned tasks and their spec criteria. `/fw-spec` tags each criterion `[<grader-type> | visible|held-out]` with a `verify:` line and a `defends against:` line; `/fw-plan` compiles each into a task `goal` plus `graders`. Learn how this repo proves a change correct, without reading the implementation of any criterion you will author.

```bash
# The planned tasks (read-only) and the spec they compile.
ls __FW_TASKS_DIR__/active/ 2>/dev/null
ls __FW_SPECS_DIR__/[0-9]*-FEATURE-*.md 2>/dev/null

# How this repo proves a change correct: the conventions and runner the oracle must match.
ls Makefile justfile package.json pyproject.toml Cargo.toml go.mod 2>/dev/null
ls -d test tests spec specs __tests__ e2e 2>/dev/null
```

Triage EVERY held-out criterion into exactly one bucket. Most routes are NOT "author an oracle", and that is the point: a held-out test for a criterion that did not need one grades nothing the un-gameable check already grades, and is one more brittle file the next operator deletes. Be ruthless; the small discriminating suite is the cheap path, the maximalist brittle suite must never accumulate.

- **AUTHOR a held-out oracle** when the criterion is flagged `held-out`, describes a BEHAVIOR, has an observable contract, AND a discriminating example or relation can be pinned from the criterion plus contract alone. These are the only criteria you fan a blind author out for.
- **SKIP (already un-gameable, or not oracle-able)** for structural / state / filesystem / schema checks (status code, body shape, type, a file exists, an exit code, a value stored): deterministic checks the agent cannot author or game, which `/fw-plan` already grades with a `command`. A schema cannot grade behavior anyway. Do not manufacture a hidden oracle for these.
- **SKIP -> route to manual** for subjective / aesthetic / human-judgment criteria (the Weyuker class: programs written to determine the answer in the first place). Forcing a hardcoded expected value here builds a false, brittle gate. `/fw-plan` already routes these to a `manual` gate; leave them there.
- **ROUTE TO MANUAL, do not silently wave through**, for a held-out behavior criterion where NO discriminating oracle can be authored (the strength gate below cannot be met, or no author can pin an example from the criterion alone). A passing strength gate is necessary, not sufficient; its absence is a routing signal, never a thing to paper over. Hand it to a `manual` reviewer with the reason and say plainly the held-out promise for that criterion is unmet.

Produce the worklist, and PRINT the routing table with an explicit gate before you write any test:

```
## Verify routing for <spec / phase / tasks>

Author held-out oracle:  #<n> "<criterion>"  (task <id>)  -- behavior, contract present, discriminator pinnable
Skip (un-gameable):      #<m> "<criterion>"  -- schema/state check, already graded out-of-band by /fw-plan
Route to manual:         #<k> "<criterion>"  -- subjective, or no discriminating oracle: <reason>

Authoring <a> held-out oracle(s) for <T> task(s). Proceed?
```

Only AUTHOR rows enter the fan-out. Everything else is a routing record you carry to the final report.

## STEP 2: BUILD THE FENCE PACK (one per AUTHOR criterion)

For each AUTHOR criterion, assemble one self-contained text block, the FENCE PACK, that IS the entire context a blind author will receive. The fence pack is the fence made concrete: building it correctly IS the enforcement, because the only way to produce a test is to hand a blind author nothing but this pack. Copy ONLY the may-see surface into it; never paste the spec wholesale, the plan, your STEP 1 notes, or anything resembling an implementation.

```
You are authoring a held-out acceptance test, BLIND to the implementation.
You have never seen the code under test and you must not ask for it, sketch it,
or assume a canonical algorithm. Write the test ONLY from the contract below.

CRITERION (the one behavior this test must discriminate):
  <the exact criterion text>
  verify:           <the spec's verify: line>
  defends against:  <the spec's defends against: line -- the gaming move to foreclose>

OBSERVABLE CONTRACT (the only surface you may assert against; declared, not imagined):
  - Signatures / entry points: <names, arguments, return shapes>
  - Input / output shapes:      <types, formats, ranges, ordering guarantees>
  - Side-effects:               <files, calls, state transitions the behavior must cause>
  - Pre / post / invariants:    <declared conditions that must hold>
  - Pinned examples:            <only the concrete examples the spec itself pins; omit if none>

REPO TEST CONVENTIONS (so the test runs and reads idiomatically here):
  - Framework / runner:  <e.g. pytest, jest, go test>
  - Fixtures to reuse:   <named>
  - One wiring example:  <a short, verbatim existing test showing imports, fixtures, naming>
  - File location:       .flywheel/verification/<spec>/ (git-ignored scratch; not committed)

WRITE:
  - A test that FAILS a plausible-wrong implementation of this criterion and PASSES
    a correct one. Prefer a metamorphic relation or property (round-trip, idempotence,
    invariant, ordering, model equivalence) over a concrete golden value -- there is no
    single value to read off, so it is harder to game. Use a concrete-value assertion
    ONLY when the contract fully pins the output.
  - Then propose a SMALL set (2-4) of plausible-WRONG reference implementations of this
    criterion (off-by-one, inverted condition, dropped side-effect, returned constant,
    wrong ordering) AND one CORRECT reference. Return the test, the wrong references, and
    the correct reference.

FORBIDDEN: you have no implementation, no reference solution, no candidate output. If you
CANNOT pin a discriminating example or relation from the criterion + contract above alone,
return UNDER-SPECIFIED and state exactly what the contract fails to state. Do NOT invent the
missing behavior, do NOT assume a standard algorithm, and do NOT assert behavior the contract
does not state.
```

**Read the pack back against the fence before you send it.** Does it leak the implementation, a reference, or your guess at the algorithm? Strip it. Is the criterion explicit and project-specific? If not, an unfillable pack IS the under-specification finding: route it upstream (CONTRACT-PINNING FEEDBACK) before a single token is spent on authoring. The fence pack is also the unit of escalation, every author and any later synthesizer receives the identical fenced surface, so escalation never widens the fence.

## STEP 3: FAN OUT THE BLIND AUTHOR (adaptive, not maximalist), AND HONOR THE TIMING RULE

**TIMING is part of the fence.** The author runs BEFORE execute, blind to any diff, the TDFlow shape (independent author; the agent only resolves later). You will NOT run a runtime-feedback repair loop that iterates the oracle against a candidate output; that re-teaches the oracle to assert observed, possibly-buggy behavior. There is no candidate at this stage, and you must not manufacture one to "check" the oracle against. If an oracle fails to author or fails the strength gate, repairs use ONLY the criterion, the contract, and the synthesized references from STEP 4, never a real or imagined implementation.

**Default to ONE blind author per criterion.** Spawn a blind-author subagent via the Task tool, handing it ONLY the fence pack. Fan-out hits diminishing-to-negative returns early (a few percent gain for an order of magnitude more cost), so do not pay it by default. The running agent may itself author a candidate directly from the pack for a single criterion, but the moment implementation context could leak, spawn a fresh blind subagent instead, the pack's construction is what keeps you blind.

**Escalate to N independent blind authors plus a synthesis pass ONLY when** the single author's oracle FAILS the strength gate (STEP 4), or when authors DISAGREE on a relation you are unsure pins the criterion. When you escalate, DIVERSIFY the model and the prompt framing across authors, not just instances, consensus across instances of one model is weak because correlated priors manufacture false agreement. Spawn the N authors in parallel, each with the SAME fence pack but a different model/prompt; an author never sees another author's output during authoring (that re-correlates them). Then run a synthesis pass yourself (or a curator subagent that is also fence-only, never implementation-aware): it does NOT vote on popularity, it WEIGHTS discrimination (passing the kill test) over mere agreement, may combine the strongest relation from several candidates, and pools every author's wrong references into one kill-set for the final gate. A holdout that passes the gate beats one three authors agreed on that kills nothing.

## STEP 4: THE DISCRIMINATION GATE, THE ORACLE MUST DISCRIMINATE

An oracle that exists is not an oracle that works: a test green on both a correct and a wrong implementation grades nothing. Before you trust any holdout, prove it discriminates, still blind, still using no real implementation.

Classic mutation testing mutates a real, existing program to grade a real, trusted suite. At fw-verify time NEITHER exists: the implementation is unwritten (the point of blindness) and the suite is what you are authoring. So you INVERT it, synthesize the references and let them grade YOUR test. This synthesize-the-wrong-references move is CREDIBLE BUT UNPROVEN: prototype it, do not assume it works for free.

For each held-out criterion you author, you bind three things together:

1. **The holdout test** itself, the metamorphic relation or assertion, written against the contract.
2. **One synthesized CORRECT reference**, a minimal implementation a careful engineer would write to satisfy the criterion. Scratch only; never committed, never shown to the implementer.
3. **A small, concrete set of synthesized PLAUSIBLE-WRONG references** scoped to THIS criterion (off-by-one, inverted condition, dropped side-effect, returned constant, wrong ordering, a round-trip that loses precision). Two to four targeted variants, NEVER an exhaustive operator sweep, NEVER random repo-wide mutation.

**The gate, applied to every holdout before it is trusted:**

- **PASS the synthesized correct reference.** Run the holdout against it; it must be green. This is MANDATORY, and it guards TWO failures at once: a test that asserts nothing (passes everything), AND a false "weak oracle" verdict that is really a synthesized wrong reference being behaviorally EQUIVALENT to correct. The pass-of-correct is what tells you the test is real.
- **KILL at least one synthesized plausible-wrong reference.** Run the holdout against each; at least one must go red, and that red must be CAUSED by the seeded bug (the discriminating input actually reaches the assertion). Constructing the discriminating input is the real work; most weak generated tests fail here, asserting near the bug but never exercising it.

**Honest tolerances, do NOT distort the gate into a number:**

- **Do not chase a 100% kill rate or a high mutation SCORE.** Score correlates weakly with real-fault detection once suite size is controlled (Papadakis), and some plausible-wrong variants are behaviorally equivalent and unkillable by any test (undecidable). The bar is "kills one concrete plausible-wrong reference and passes the correct one", not a percentage.
- **Tolerate a non-zero unkillable fraction.** Accepting a false positive (an unkillable equivalent variant) is cheaper than discarding a good test; always keep the correct-reference pass-check alongside it. The kill of ONE genuine wrong variant is what tells you the test discriminates; a no-kill result is then honestly attributable to either an equivalent mutant OR a genuinely non-discriminable criterion, the latter routing to manual.
- **Do NOT weaken your synthesized wrong references until one happens to die, that games your own gate.** Either find a discriminating input (escalate per STEP 3) or, if a real attempt plus escalation still kills nothing, the criterion is not discriminable here, route it to manual (STEP 1's last bucket). Never silently ship it green.

**Record the discrimination proof:** which wrong reference died, on which input, and that the correct reference passed. That evidence is the durable artifact of this stage. Discard the synthesized references themselves as throwaway scratch the moment the gate is recorded, they MUST NOT ship into the repo or the agent's view; a reference solution in the worktree is exactly the leak the fence forbids.

## STEP 5: SCREEN FOR EXECUTABILITY AND FLAKINESS, THEN RECORD, REGISTER, AND FENCE

A generated holdout is measurably flakier and more often non-executable than a human one, and a non-deterministic verdict destroys the authoritativeness the whole stage exists for. Screen every gate-passing holdout BEFORE you trust it to grade, against the synthesized correct reference from STEP 4, never the candidate:

- **Executability first.** It must compile/parse and run under this repo's runner. A test that cannot execute detects nothing, fix it (blind, from the contract) or route the criterion to manual.
- **Flake screen.** Run it at least twice (run-twice / run-N) against the synthesized correct reference. Identical verdict each time, or QUARANTINE it: remove the nondeterminism (pin time, seed, ordering, I/O) or, if it cannot be removed, route the criterion to manual. A flaky holdout is worse than none, it fails correct implementations at random.

Write each passing, discriminating, non-flaky holdout to the GIT-IGNORED verification scratch location `.flywheel/verification/<spec-or-phase>/`, separated from the agent's visible tests and NEVER committed: the directory is git-ignored, so the oracle does not land in the tree, never appears in a freshly provisioned worker worktree, and never reaches the implementing agent's view. One holdout per criterion, named for the criterion, not the implementation. The holdout adds NO requirement beyond the declared criterion: a hidden oracle that demands behavior the contract does not state is itself a defect.

Because this stage runs BEFORE execute, the holdout grades the SYNTHESIZED references from STEP 4 (the discrimination gate), not a real implementation that does not exist yet. Its job here is to PROVE blind that a discriminating oracle exists for the criterion and to RECORD that proof — not to gate the real agent run. The execute-time gate on the agent's actual work stays where `/fw-plan` put it: the task's own `command` graders plus the tests the implementing agent must write into the repo's normal suite (the durable, CI-run regression guard). Do NOT wire this throwaway oracle in as an in-repo grader — a git-ignored path is absent from the worker's worktree, and committing it to make the grader resolve would re-open the in-repo gameability this relocation exists to close. That prohibition targets the in-repo / in-worktree path (a committed oracle is in the agent's view, gameable). It does NOT forbid the DIFFERENT, sanctioned channel below: writing a registration to the configured out-of-worktree held-out root, which the orchestrator reads from OUTSIDE the worktree and never materializes into the agent's view. Registration is a write to a git-ignored root, never a commit into the tracked tree.

Then RECORD the durable artifact and PRESENT the one task-side edit (you READ tasks; the operator OWNS the task edit, present it, do not write the task JSON yourself):

- **Record the discrimination proof** for each admitted holdout under `__FW_AUDITS_DIR__`: which synthesized wrong reference died, on which input, that the correct reference passed, and the flake-screen result. This recorded evidence — not a committed test file — is the artifact this stage keeps.
- **Register the admitted oracle at the held-out root (the sanctioned out-of-worktree channel).** When the repo configures a held-out root — the `[held_out] root` key in `flywheel.toml`, conventionally a git-ignored directory under `.flywheel/verification/` — write a registration to `<held-out-root>/<task_id>.json`, KEYED BY the owning task id: a held-out `command` grader that invokes THIS admitted oracle BY ITS ABSOLUTE OPERATOR PATH, run with the agent's committed tree as its working directory. This is the channel that lets the discrimination you proved blind gate the agent's REAL run out-of-band: the orchestrator reads `<held-out-root>/<task_id>.json` from OUTSIDE the worktree, so neither the registration nor the oracle it references ever appears in the agent's view. Reference the oracle by absolute path — a relative path would resolve against the worktree, where the oracle does not exist. This registration is a write to the git-ignored, out-of-worktree root ONLY; it is NOT a commit into the tracked repo and NOT an in-repo `command` grader (the fence below still holds — the two are distinct moves). If no `[held_out] root` is configured there is nowhere to register: record the proof and present the fence as before, and note the registration cannot land until a held-out root is set.
- **A `non_goals` line** forbidding the implementing agent from reading or writing the verification scratch dir ("do not read or write under `.flywheel/verification/`"), so a future blind oracle for the same criterion is not leaked into the agent's view. Add the path to the task's protected paths if the work source carries one.

```
## Held-out oracles authored for <spec / phase / tasks>

#<n> "<criterion>"  ->  .flywheel/verification/<spec>/<file>  (task <id>)   [git-ignored, uncommitted]
   form: metamorphic (round-trip) | property | concrete-value
   discrimination proof: killed <wrong-ref> on <input>; passed the correct reference
   screened: executable [pass], discriminates [pass], flake-screen run-twice [stable]
   recorded: __FW_AUDITS_DIR__/<record>      (durable artifact)
   registered: <held-out-root>/<task_id>.json   (when [held_out] root is set; oracle by absolute path, cwd = committed tree; git-ignored, out-of-worktree — NOT committed)
   fence to add to task <id> (operator applies):
     non_goals: "Do not read or write under .flywheel/verification/"
```

## CONTRACT-PINNING FEEDBACK (the upstream loop, do not skip it)

When a blind author (or you, building the pack) cannot pin a discriminating example or relation from the criterion plus contract ALONE, that is NOT a failure to route around with a guess, it is the signal that the criterion is under-specified. Writing the test illuminated a missing or ambiguous requirement; that is the loop working, not breaking. Surface it back to `/fw-spec` (and `/fw-plan`):

- Report the criterion, the owning task, and the SPECIFIC thing the contract fails to state (the unpinned input, the unstated output relation, the ambiguous boundary, the undefined empty-input or rounding rule).
- Do NOT fabricate an oracle, do NOT hallucinate a canonical spec, and do NOT demand behavior the declared contract never stated (the inverse defect, the `edit_only` lesson: a hidden oracle requiring unstated behavior is itself a bug). Keep every author pinned to the DECLARED contract.
- Pin concrete examples from the spec where it offers them, but always back them with the discrimination gate, an under-constrained example can pass while validating nothing.
- The criterion stays in the manual-review bucket until the spec is sharpened upstream; it never ships a green-by-default holdout.

```
## Under-specified criteria (route back to /fw-spec)

#<k> "<criterion>"  (task <id>)  -- cannot pin a discriminating oracle blind.
   missing from contract: <the exact declaration needed -- an expected value, an invariant, a side-effect, an error case>
   -> sharpen in __FW_SPECS_DIR__/NNNNN-FEATURE-<name>.md, then re-run /fw-plan and /fw-verify.
```

## STEP 6: PRESENT AND HAND OFF

Show the operator what is now provable, where it is honest about its own limits, and what bounced upstream. Do not summarize prose, summarize what each held-out test discriminates.

```
Held-out oracles authored: <a> for <T> tasks (blind, before execute, discrimination-proven)

Per criterion authored:
  <task-id> / criterion <n>: <metamorphic|property|concrete> holdout
    located: .flywheel/verification/<spec>/<file>   (git-ignored, uncommitted)
    discriminates: killed <wrong-ref> on <input>, passed the correct reference
    recorded: __FW_AUDITS_DIR__/<record> (durable proof); fence to add to <task-id>.non_goals (operator applies)
    registered: <held-out-root>/<task-id>.json when [held_out] root is set (oracle by absolute path; out-of-worktree, git-ignored — never committed)

Skipped (already un-gameable): <s>          (structural/state/filesystem/schema)
Routed to manual: <m>                       (subjective, or no discriminating oracle authorable)
Returned upstream as under-specified: <u>   (criterion + the unstated contract detail)

This stage proves blind that a discriminating oracle EXISTS and records that proof. The
execute-time gate is always the task's own command graders and tests (the durable, CI-run
guard). When a `[held_out] root` is configured, the registration written above ALSO routes
the admitted oracle to the orchestrator's out-of-worktree HELD-OUT gate — the agent graded
by a test it never saw — which the orchestrator runs from outside the worktree; absent that
config, no held-out gate runs. fw-verify only emits the registration; activating and running
the gate is orchestrator-owned. A held-out suite is a filter, not a correctness guarantee.

Next: the operator adds the non_goals fence to each task, ensures `[held_out] root` is set if
the registration should gate the run, then runs the tasks with `flywheel worker` (graded by
the task's own command graders and tests, plus the held-out gate when configured). Return the
<u> under-specified criteria to /fw-spec before executing.
```

## ANTI-PATTERNS

- **DO NOT** let the fence pack contain the implementation, a sketch of it, the agent's visible tests, any runtime/compiler output, or a reference solution, each one re-teaches the oracle to certify buggy behavior. Read the pack back against the fence before sending.
- **DO NOT** iterate the oracle against a candidate's produced output to make it pass, that aligns the assertion to observed behavior. Repairs use only the criterion, the contract, and synthesized references.
- **DO NOT** trust an oracle that merely exists, every holdout must KILL a plausible-wrong reference AND PASS a correct one before it grades, and that kill-and-pass is the proof you record.
- **DO NOT** chase a 100% kill rate or a mutation score; tolerate unkillable equivalent variants and keep the correct-reference pass-check.
- **DO NOT** weaken your synthesized wrong references until one dies, that games your own gate; find a discriminating input or route to manual.
- **DO NOT** author a held-out oracle for structural/state/filesystem/schema checks (already un-gameable) or subjective criteria (route to manual). A held-out test for a criterion that did not need one grades nothing.
- **DO NOT** silently wave through a held-out behavior criterion you cannot author a discriminating oracle for, route it to manual review.
- **DO NOT** fabricate an oracle or assume a canonical algorithm when the criterion is too thin, surface it upstream as under-specification, pinned to the declared contract.
- **DO NOT** demand behavior the declared contract does not state, a hidden oracle requiring unstated behavior is itself a defect.
- **DO NOT** ship a generated holdout without an executability and run-twice flake screen, a non-deterministic verdict destroys authoritativeness.
- **DO NOT** fan out N authors by default; escalate adaptively only on a failed strength gate or genuine disagreement, and diversify models/prompts when you do.
- **DO NOT** ship the synthesized reference implementations into the repo or the agent's view, a reference in the worktree is a leaked answer.
- **DO NOT** edit the spec or a handed-off task's goal or graders; write the oracle only into the git-ignored `.flywheel/verification/` scratch dir, record its discrimination proof under `__FW_AUDITS_DIR__`, write the `<held-out-root>/<task_id>.json` registration to the configured held-out root when `[held_out] root` is set, and PRESENT the `non_goals` fence for the operator to apply.
- **DO NOT** land the throwaway oracle into the committed repo or wire it as an in-repo `command` grader — a git-ignored path is absent from the worker's worktree, and committing it to make a grader resolve re-opens the in-repo gameability this relocation closes. Writing the registration to the out-of-worktree, git-ignored held-out root is a DIFFERENT, sanctioned move (the orchestrator reads it from outside the worktree); it is never a commit into the tracked tree. The in-repo execute-time gate stays the task's own graders and tests.
- **DO NOT** claim this stage gates the agent's real run or that the held-out flag is tamper-proof: it proves blind that a discriminating oracle exists and records that proof; an execute-time held-out gate additionally needs out-of-worktree grading owned by the orchestrator. Say so plainly.
- **DO NOT** use emojis.
