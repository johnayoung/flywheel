# Feature: Autopilot (MVP)

## Outcome
Running `flywheel autopilot` against any codebase keeps the work queue non-empty
with verifiable, tier-prioritized tasks: a fan-out of one agent per tier judges
which tiers are even relevant to *this* repo and surfaces concrete findings, a
synthesis agent sequences them by the tier model (preemptive 1-3 above weighted-
scored 4-11), and each selected finding is compiled — entirely by agents, with no
human interview — through the authoring pipeline into a flywheel task that carries
a real grader. The existing worker drains and FF-merges that work, and autopilot
refills as the queue drains, so flywheel never sits idle on a repo that still has
work, and never invents work on one that does not.

## Background
flywheel already executes graded tasks autonomously (worker + worktree + submit)
and already has a `WorkSource` seam, scheduling `priority`, and a held-out landing
gate. What it lacks is the *intake* half running unattended: today a human runs the
interactive `/fw-spec` and `/fw-plan` skills to author tasks. Autopilot is the loop
that closes that gap — it discovers what a codebase needs and authors gradeable
tasks for it without a human in the path. The load-bearing tacit requirement
surfaced in the interview: the tier model must be judged *per codebase by an agent*,
not by coded detectors — "production down" is meaningless for a library, and only an
agent reading the repo can decide a tier is irrelevant. The second: "done by agents"
includes the authoring rigor — ambiguity that `/fw-spec` would ask a human about is
instead resolved by the authoring agent as a recorded assumption, never by emitting a
vaguer task. The integrity line that cannot move: autopilot may decide *what* to do,
but it never gets to decide that its own work *succeeded* — every emitted task lands
only on an out-of-band grade.

## Scope
### In scope
- A `flywheel autopilot` entry that runs the discover -> sequence -> author -> refill
  loop against the repo's configured work source and store.
- One relevance-and-discovery agent invocation per tier (the 11 tiers in
  `docs/autopilot.md`), fanned out, each returning a structured "relevant to this
  repo?" verdict plus zero or more findings with evidence.
- A synthesis step that assigns/confirms each finding's tier, computes the legible
  weighted score from `docs/autopilot.md`, applies the preemptive 1-3 override, and
  selects findings to fill the queue to a target depth.
- Headless authoring: each selected finding is compiled by agents through the
  `fw-spec` -> `fw-plan` contracts (ambiguity resolved as recorded assumptions) into
  one or more validated flywheel tasks, each with at least one grader.
- Emitting those tasks into the configured work source so the existing worker runs
  them, with FF-merge autonomy as the shipped default.
- A recorded, inspectable score breakdown (tier, urgency, importance, blocks, effort,
  final) on every emitted task.

### Out of scope
- Detecting Tier 1/2/4 from live external signals (incident feeds, paging,
  contracts). The tier *agents* may surface such work if the repo contains evidence
  of it, but autopilot ships no incident/deadline integration.
- New grader *types*. Autopilot composes existing grader types only.
- A real-time UI for autopilot. Observability is the recorded score breakdown plus
  existing `flywheel status`/logs; no new dashboard.
- Tuning the weights to "correct" values. Defaults ship; the weights are config knobs
  and are expected to be wrong at first.
- Replacing the interactive `/fw-spec` / `/fw-plan` skills. They remain for
  human-driven authoring; autopilot is the unattended path.

### Must not regress
- The worker / orchestrator drive an operator-authored `.flywheel/tasks` directory
  exactly as before when autopilot is not run.
- `import flywheel_core` continues to work without the agent SDK installed; all
  agent-driving stays behind the lazy `flywheel_core._sdk` boundary.
- `protected_paths` and the submit-time rebase + grader re-run still gate every
  landing; autopilot work lands through the same seam, not around it.

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its grader
type, visibility, the exact check, and the gaming move it forecloses. `/fw-plan`
lowers each one to a command / transcript / rubric / manual grader. The agentic
stages are graded deterministically by driving the pipeline with a *scripted agent
invoker* (the existing loop-path test pattern), so the criteria do not depend on
live-model nondeterminism; one end-to-end criterion drives the real loop.

1. When autopilot emits a task, that task validates against the core `Task` schema
   and carries at least one grader. [command | held-out]
   verify: load every file autopilot wrote into the work source through
   `flywheel_core.loaders.load_task_file`; assert each loads without error and its
   `graders` is non-empty. A finding that cannot be compiled to a grader-bearing task
   is dropped with a recorded reason, never written as a task.
   defends against: emitting prose, notes, or a goal-only stub as a runnable task so
   the worker has nothing to verify and the executing agent self-declares done.

2. When autopilot runs against a codebase where a tier does not apply, that tier
   contributes zero tasks and records an explicit not-relevant verdict for it.
   [command | held-out]
   verify: run autopilot with a scripted invoker over a library fixture whose tier
   agents return "not relevant" for the production/deploy tiers; assert zero tasks
   carry those tiers and a recorded per-tier relevance verdict marks them
   not-relevant with a reason.
   defends against: emitting one boilerplate task per tier regardless of fit, i.e.
   faking "full coverage" by treating every tier as always-applicable.

3. Every emitted task records a score breakdown whose components (tier weight,
   urgency, importance, blocks-count, effort) recompute to the recorded final score
   under the formula in `docs/autopilot.md`. [command | held-out]
   verify: for each emitted task, re-evaluate the documented score formula from the
   recorded components and assert it equals the recorded final score; assert all five
   components are present. The agent supplies urgency/importance/effort estimates; the
   final score is computed by autopilot, not reported by the agent.
   defends against: an opaque agent-reported single number that nobody can audit, or a
   final score that does not match its stated components.

4. When at least one ready Tier 1-3 finding and one or more Tier 4-11 findings are
   present, every ready Tier 1-3 finding is sequenced ahead of every Tier 4-11
   finding, regardless of the scheduled findings' scores. [command | held-out]
   verify: feed a fixed finding set (one ready Tier-3 broken-build finding plus
   several high-scoring Tier-5/Tier-8 findings) through the synthesis step with a
   scripted invoker; assert the selection/sequence places the Tier-3 item before any
   Tier 4-11 item.
   defends against: a cheap high-scoring polish item (Tier 11) outscoring an open
   broken build because the weighted formula was applied to a preemptive tier.

5. Among Tier 4-11 findings, selection follows the weighted score, not strict tier
   order — a higher-scoring lower tier is selected before a lower-scoring higher tier.
   [command | visible]
   verify: feed a fixed set where a Tier-8 finding's computed score exceeds a Tier-5
   finding's; assert the Tier-8 is selected first.
   defends against: reverting to strict top-down priority, which starves docs/debt/
   tests forever — the exact failure `docs/autopilot.md` exists to prevent.

6. While the queue depth is below the configured target and at least one actionable
   finding exists, autopilot emits tasks until the target is met or actionable
   findings are exhausted; when no finding is actionable it emits zero tasks and exits
   0 without error. [command | held-out]
   verify: (a) with a scripted invoker over a fixture that yields known findings and
   an empty queue, assert autopilot raises the queue to the target depth (or to the
   number of available findings, whichever is smaller); (b) with a fixture whose tier
   agents all return not-relevant / no findings, assert zero tasks written and exit
   code 0.
   defends against: spinning or erroring (or inventing filler work) on a clean repo;
   and never refilling a drained queue.

7. When autopilot runs the full pipeline against a fixture repo containing a real
   defect, the emitted task's grade is the repo's out-of-band verification command,
   the worker drives the task to a green grade, and the work FF-merges into the base —
   proven by driving the real `orchestrate` loop, not a unit of it. [command |
   held-out] (in-loop-verification)
   verify: an in-loop test seeds a temp git repo with a failing check, runs autopilot
   to author the task, runs the real worker/orchestrate loop with a scripted executing
   invoker that fixes the defect, and asserts the check goes red -> green and the
   branch FF-merged into the base.
   defends against: unit tests that prove generation in isolation while the live loop
   never executed the new path (the documented reason the archive gate exists).

8. The authoritative grader on an emitted task is a check the *executing* run did not
   author against its own known inputs — it is the repo's existing verification
   command or a held-out check, never a brand-new check created inside the run it
   grades. [command | held-out] (verification-surface)
   verify: for each emitted task, assert its authoritative grader command resolves to
   a pre-existing repo check or a registered held-out oracle, and that the task does
   not carry a grader whose target file the same task's diff creates.
   defends against: the "entirely by agents" authoring path emitting a task whose
   grader is a test the executing agent writes to pass itself — self-attestation
   wearing a grader's clothes.

Verification surface: changed. Autopilot adds a generative path that *authors* the
graders other work is judged by, so it inherits a standing Definition-of-Done: the
repo's existing verification suite still runs and still passes after any autopilot-
landed change (enforced out-of-band, criterion #7); any check an autopilot task would
relax, remove, or skip is refused by the existing `protected_paths` seam (criterion in
"Must not regress"); and the authoritative grade that lands autopilot work is always
out-of-band or held-out, never a check the executing run authored against its own
known inputs (criterion #8). A removed or weakened repo check with no equal-or-greater
replacement is a blocking defect, caught by #7's green-suite assertion.

## Decomposition Hint (for /fw-plan)
Splits along these layers; chain with prerequisites so shared invariants land before
their dependents.

- Layer scoring (pure, no I/O, no agent): the Tier enum (1-11) with its preemptive/
  scheduled split, the `ScoreBreakdown` value, the weighted-score formula, and the
  preemptive override + selection-to-target-depth logic. Satisfies #3, #4, #5.
  Pure-function, fully unit-gradeable; depends on nothing.
- Layer discovery (agent fan-out, behind `flywheel_core._sdk`): one relevance-and-
  findings agent per tier, returning a structured verdict + findings; testable with a
  scripted invoker. Satisfies #2. Depends on the Tier enum from the scoring layer.
- Layer authoring (agent, headless): compile a selected finding through the
  `fw-spec`/`fw-plan` contracts into validated grader-bearing tasks, resolving
  ambiguity as recorded assumptions. Satisfies #1, #8. Depends on discovery's Finding
  shape.
- Layer loop+surface (orchestrator/worker + product shell): the `flywheel autopilot`
  entry, queue-depth target + refill + idle-safe exit, config (`[autopilot]` in
  `flywheel.toml`, FF-merge default), emission into the work source. Satisfies #6.
  Depends on scoring + authoring.
- Layer end-to-end (in-loop-verification): drive autopilot -> real orchestrate loop ->
  FF-merge on a fixture defect. Satisfies #7. Depends on every layer above.

Shared invariants multiple layers assert against — land them in the layer that owns
them and have dependents import, never redefine:
- The `Tier` enum (1-11) and the preemptive (<=3) vs scheduled (>=4) boundary.
- The `ScoreBreakdown` structure (tier, urgency, importance, blocks, effort, final)
  and the documented formula — #3 and the config layer both bind to it.
- The per-tier relevance verdict and `Finding` structured-output shape — discovery
  emits it, authoring and #2 consume it.

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-1: Tiers are judged per-codebase by agents, not coded detectors  (Status: Accepted)
- Context: tier relevance is codebase-specific ("production down" is meaningless for a
  library); coded detectors would need a signal source per tier and would misfire
  across repo shapes. Decision: one agent per tier decides relevance and surfaces
  findings by reading the repo; a synthesis agent sequences the summaries.
- Rejected: coded per-tier detectors + config (rigid, needs signals flywheel lacks,
  the user explicitly steered away from it); a single mega-prompt doing all tiers at
  once (loses the parallel fan-out and per-tier relevance isolation). Consequences:
  discovery cost scales with agent invocations; relevance verdicts are
  nondeterministic, so criteria grade them via scripted invokers, not live models.

### D-2: Authoring is the full pipeline, run entirely by agents  (Status: Accepted)
- Context: emitted tasks must be ungameable-gradeable, which is exactly what the
  human-run `fw-spec`/`fw-plan` pipeline guarantees. Decision: autopilot runs that same
  pipeline headlessly; ambiguity a human would be asked about is resolved by the
  authoring agent as a recorded assumption, never by emitting a vaguer task.
- Rejected: auto-attaching only the repo's green-suite command as a blanket grader
  (simpler, but loses per-task specificity and the authoring rigor the user wants);
  pausing for a human at each refill (breaks the unattended loop). Consequences: the
  authoring agent's judgment is load-bearing; #8 and the verification-surface DoD exist
  precisely to keep an agent-authored grader from becoming self-attestation.

### D-3: FF-merge autonomy is the shipped default  (Status: Accepted)
- Context: the user wants to run autopilot unattended on a real codebase and see it
  never stop. Decision: autopilot work lands via FF-merge by default, bounded only by
  the existing submit-time rebase + grader re-run + `protected_paths` seam.
- Rejected: PR-landing default (safer but a human is in every landing, blunting
  "never stops"); propose-only first run (defers the actual demonstration).
  Consequences: unattended writes to the base branch from the first run; the integrity
  burden falls entirely on the out-of-band grade (#7, #8) and protected_paths.

### D-4: "Never stops" means never blocked, not always busy  (Status: Accepted)
- Context: on a clean repo with no actionable work, forcing the queue full would invent
  low-value work. Decision: autopilot fills to a target depth only from actionable
  findings; when none clear the bar it emits nothing and exits 0, re-surveying on the
  next run.
- Rejected: always-keep-N-queued (invents filler on clean repos); escalate-to-operator
  on empty (adds a human gate to an unattended loop). Consequences: a genuinely clean
  repo produces an empty, exit-0 run — graded explicitly by #6(b).

## Open Questions (accepted gaps)
- Whether an agent-authored grader is *provably* ungameable in the general case is an
  open research question; the MVP bounds the risk structurally (#8 + verification-
  surface DoD: authoritative grade is out-of-band/held-out and never a check the
  executing run authored) rather than proving the general claim. Accepted for the MVP.

## Next Steps
Run `/fw-plan 00058-FEATURE-autopilot-mvp` to compile these criteria into flywheel
tasks and graders.
