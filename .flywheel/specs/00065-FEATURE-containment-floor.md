# Feature: Containment Floor (no error escapes a loop)

## Outcome
No single exception — raised by a work source, an individual work item, a
grader subprocess that cannot start, DAG construction, or held-out gate
evaluation — can crash a daemon loop or abort an entire scheduling pass. The
failing unit (one source listing, one item, one task, one grader, one gate
call) is contained, recorded with a reason, and the surrounding loop or pass
proceeds with the remaining valid work. This is Phase 1 of the
"autopilot never stops" reliability program: the containment floor every later
phase builds on.

## Background
Today several call sites in the orchestrator and core let a single error escape
its blast radius and kill a whole loop or pass. The asymmetry is the tell: the
worker daemon already wraps its cycle body in a consecutive-failure circuit
breaker (`flywheel_worktree/worker.py:2117`), the source reconciler and the
`source_syncs` writer already wrap `list_work()` in `try/except` and continue
(`_orchestrate.py:256`, `:320`), and the recheck path already guards its grader
`subprocess` spawn with `except OSError` (`harness.py:4049`). But the autopilot
daemon's `run_cycle()` is unguarded (`_autopilot_run.py:224`), the orchestrate
main loop's `list_work()` is bare (`_orchestrate.py:850`), every shipped work
source aborts its whole listing on the first malformed item, `WorkGraph.build`
raises on one structurally-invalid task and poisons every unrelated task in the
pass, the main-path command-grader `Popen` is unguarded (`grader_command.py:143`),
and the held-out landing gate call site is unguarded (`_orchestrate.py:1441`).
The tacit requirement a literal agent would miss: "contained" means the loop
*observably keeps going past the bad unit AND the bad unit is recorded* — not
that the exception is swallowed into silence (a bare `except: pass` that drops
the count or the surviving items is the cheapest fake and is explicitly a
defect here).

## Scope
### In scope
- Autopilot daemon cycle body gains a circuit breaker mirroring the worker's:
  a raising `run_cycle()` is counted, backed off, and on a bounded consecutive
  count the loop gives up by surfacing (a visible signal / non-zero result),
  never a silent process exit.
- Orchestrate main loop contains a raising source `list_work()` so the pass
  degrades (skips that pass's work) instead of crashing the driver.
- Each shipped work source (directory, github issues, github_ci, github_review)
  skips an individual unparseable/invalid item — counting and logging it — and
  still returns the remaining valid items.
- DAG construction isolates a structurally-invalid task (duplicate id,
  self-dependency, cycle) with a recorded reason and still schedules the
  remaining valid tasks in the pass.
- A command grader that cannot *start* (the subprocess spawn raises, e.g.
  `OSError`) is classified as a retryable internal error, distinct from a
  grader that runs and exits non-zero (a validation failure).
- The held-out landing gate call site is wrapped so an unexpected exception
  from gate evaluation cannot unwind the orchestrate pass.

### Out of scope
- Universal deadlines / timeouts on cycles or graders (Phase 2).
- Transient-failure retry/backoff *classification* beyond the grader-cannot-start
  distinction here (Phase 3).
- Surfacing or re-driving stopped work to operators (Phases 4-5).
- Process supervision / respawn of crashed daemons (Phase 6).
- Resource hardening (Phase 7).
- Changing what a *successfully-run* grader decides, or the held-out gate's
  internal fail-closed verdict logic — only the *call site* is wrapped.

### Must not regress
- `flywheel_core.task` and `flywheel_core.lifecycle` stay pure (no json / pathlib
  / io); any file/JSON logic stays out of them.
- The harness remains the authority on lifecycle transitions; agent self-reports
  stay untrusted; the authoritative grade stays out-of-band.
- A grader that runs and fails still drives the existing validation-failure
  path (it must NOT be reclassified as an internal error).
- The full gate (`scripts/check.sh`: ruff -> pyright -> pytest) stays green.
- The worker daemon's existing circuit breaker and the reconciler/sync
  `list_work()` guards keep their current contain-and-continue behavior.

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its grader
type, visibility, the exact check, and the gaming move it forecloses.

1. When the autopilot daemon's per-cycle `run_cycle` raises on a cycle, the
   daemon shall run a further cycle rather than exit on that exception.
   [command | held-out]
   verify: a new pytest under `packages/flywheel-orchestrator/tests/` drives the
   daemon's loop with an injected `run_cycle` scripted to raise once then
   succeed, an injected stop after a fixed cycle budget, and asserts the loop
   ran a cycle AFTER the raising one (cycle count past the failure) and returned
   normally; plus `scripts/check.sh`.
   defends against: catching the exception but then breaking/returning so the
   "next cycle" never runs — the assertion is on a cycle observed strictly after
   the failing one, not merely that no traceback escaped.

2. When the autopilot daemon's `run_cycle` raises on a bounded number of
   consecutive cycles, the daemon shall stop by returning a non-zero / visible
   give-up signal rather than exiting silently. [command | held-out]
   verify: a pytest drives the loop with `run_cycle` scripted to raise on every
   cycle and asserts the loop terminates by surfacing a give-up (a non-zero
   return code or an equivalent recorded give-up signal the test reads), and
   that the recorded consecutive-failure bound was reached; plus `scripts/check.sh`.
   defends against: an unbounded retry loop that never gives up (hiding a hard
   failure forever), or a give-up that exits 0 so a supervisor reads the crash
   as a clean stop — the check asserts BOTH a bounded count and a non-success
   signal.

3. If the orchestrate main-loop source `list_work()` raises, then the main
   driver shall contain the exception and the pass shall not propagate it as a
   crash. [command | held-out]
   verify: a pytest drives the orchestrate main path with a `WorkSource` whose
   `list_work()` raises, and asserts the driver call returns/continues without
   re-raising (no exception escapes the pass) while leaving any in-flight state
   untouched; plus `scripts/check.sh`.
   defends against: narrowing the guard to one exception subtype so an unrelated
   raise still escapes — the test raises a generic `Exception` from the source,
   matching the posture the reconciler/sync guards already use.

4. When a directory work source encounters one task file it cannot load while
   other valid task files are present, the source shall return the valid items
   and exclude the bad one. [command | held-out]
   verify: a pytest builds a directory of task files where exactly one is
   malformed and the rest are valid, lists the source, and asserts the returned
   items are exactly the valid ones (bad one absent, every good one present) and
   that the skip was recorded/counted; plus `scripts/check.sh`.
   defends against: returning an empty/partial list and calling it "handled"
   (which a downstream reconciler reads as "all work vanished") — the test
   asserts every valid item is still present, not merely that no exception rose.

5. When a github-issues work source encounters one issue payload it cannot
   compile into a valid item while other valid issues are present, the source
   shall return the valid items and exclude the bad one. [command | held-out]
   verify: a pytest feeds the github source a fixture of issue payloads where
   exactly one is malformed (e.g. missing the integer `number`, or an invalid
   spec block) and the rest valid, and asserts the listing returns exactly the
   valid items with the bad one skipped and counted; plus `scripts/check.sh`.
   defends against: aborting the whole listing on the first bad issue (today's
   behavior) so a single bad ticket blanks the whole queue — the test requires
   the surviving valid issues in the result.

6. When a github_ci work source encounters one run payload it cannot compile
   into a valid item while other valid runs are present, the source shall return
   the valid items and exclude the bad one. [command | held-out]
   verify: a pytest feeds the github_ci source a fixture of run rows where
   exactly one is malformed (e.g. missing a required identity string) and the
   rest valid, and asserts the listing returns exactly the valid items with the
   bad one skipped and counted; plus `scripts/check.sh`.
   defends against: a parse break on one CI run reading as a green repo (no work)
   — the test asserts the other failing runs still surface as items.

7. When a github_review work source encounters one review-thread node it cannot
   compile into a valid item while other valid threads are present, the source
   shall return the valid items and exclude the bad one. [command | held-out]
   verify: a pytest feeds the github_review source a GraphQL-shaped fixture where
   exactly one thread node is malformed and the rest valid, and asserts the
   listing returns exactly the valid items with the bad one skipped and counted;
   plus `scripts/check.sh`.
   defends against: one bad node blanking the whole unresolved-thread listing —
   the test requires the surviving valid threads in the result.

8. When a scheduling pass is built from a set of tasks containing one
   structurally-invalid task (a duplicate id, a self-dependency, or a member of
   a cycle) alongside otherwise-valid tasks, the pass shall exclude the invalid
   task with a recorded reason and still schedule the remaining valid tasks.
   [command | held-out]
   verify: a pytest builds the work graph / runs the scheduling-selection path
   over a set that mixes one structurally-invalid task with several valid,
   independent ones, and asserts the valid tasks remain schedulable (present in
   the ready/selectable set) while the invalid task is excluded and its
   exclusion reason is recorded; plus `scripts/check.sh`.
   defends against: dropping the entire pass (today's raise) OR silently dropping
   the invalid task with no recorded reason — the test asserts BOTH that valid
   tasks survive AND that the exclusion is recorded with a reason naming the
   offender.

9. If a command grader's subprocess fails to start (the spawn raises), then the
   harness shall classify that grader run as a retryable internal error rather
   than a validation failure. [command | held-out]
   verify: a pytest invokes the command-grader runner with a grader whose spawn
   raises (e.g. an injected `OSError` at the `Popen` boundary, or a grader whose
   `cwd` cannot be entered) and asserts the resulting record/outcome is the
   internal-error class (retryable), NOT the validation-failed class; and a
   sibling assertion that a grader which DOES start and exits non-zero still
   yields the validation-failed class; plus `scripts/check.sh`.
   defends against: collapsing both failures into one bucket — the test pins the
   discrimination: spawn-failure -> internal-error/retryable, ran-and-failed ->
   validation-failed. A grader that prints a success string cannot satisfy this
   because the check asserts on the classified outcome, not on grader stdout.

10. If held-out landing gate evaluation raises an unexpected exception at its
    orchestrate call site, then the orchestrate pass shall contain the exception
    rather than unwind. [command | held-out]
    verify: a pytest drives the orchestrate landing path with a gate evaluator
    injected to raise an unexpected exception, and asserts the pass does not
    re-raise (the surrounding pass continues / settles) and that landing is not
    silently completed off an unevaluated gate — i.e. an errored gate does not
    fall through to a merge/PR submit; plus `scripts/check.sh`.
    defends against: a "contain it" implementation that swallows the gate error
    and then proceeds to land unverified — the test asserts the submit/landing
    effect is NOT taken when the gate evaluation itself errored (fail-closed at
    the call site, mirroring the gate engine's internal discipline).

Verification surface: unchanged. This feature adds new tests under
`packages/*/tests/` and hardens production call sites; it does not relax,
remove, or weaken any existing test, assertion, lint, or typecheck. The full
gate (`scripts/check.sh`) must still pass after every task, and the existing
worker-breaker / reconciler-guard / recheck-guard tests must stay green
unchanged (no relaxed assertions). Each new behavior is proven by an out-of-band
`command` grader (a pytest the harness runs outside the agent's turn) the agent
must not weaken.

## Decomposition Hint (for /fw-plan)
Each finding is an independent, single-file (or near-single-file) containment at
a distinct call site; they share no runtime invariant, so they are wide-and-shallow
with almost no cross-edges. Group along the module a fix lives in, one observable
behavior per task:
- Layer core/grader (`flywheel_core.grader_command` + harness classification):
  satisfies #9. No dependency on the others.
- Layer source-listing (one task per shipped source — directory, github,
  github_ci, github_review): satisfies #4, #5, #6, #7. Each is an independent
  source adapter with its own test fixture; no cross-edges between them.
- Layer graph-build (`flywheel_orchestrator._work_graph` / scheduling selection):
  satisfies #8. The orchestrate main-loop containment (#3) consumes the graph
  build, so #3 depends on #8 (the main loop must keep running when the graph it
  builds isolates an invalid task instead of raising).
- Layer orchestrate-driver (`flywheel_orchestrator._orchestrate` main loop +
  gate call site): satisfies #3 (depends on #8), and #10 (independent gate-site
  guard).
- Layer autopilot-daemon (`flywheel_orchestrator._autopilot_run` loop):
  satisfies #1, #2 (one cohesive breaker — both criteria are facets of the same
  circuit breaker, so one task with two graders).
Shared invariant: the worker daemon's breaker shape
(`MAX_CONSECUTIVE_CYCLE_FAILURES` / `CYCLE_FAILURE_BACKOFF_SECONDS` /
give-up-with-visible-signal in `flywheel_worktree/worker.py`) is the reference
the autopilot breaker mirrors — the daemon task must not invent a divergent
breaker contract.

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-1: Contained means proceed-AND-record, never silently swallow  (Status: Accepted)
- Context: A bare `except: pass` makes every criterion trivially "pass" while
  destroying the signal (an empty listing reads as "all work vanished"; an
  unrecorded drop hides the loss). | Decision: Every containment must both (a)
  let the loop/pass observably proceed past the failing unit AND (b) record the
  failing unit (count + reason/log) so it is recoverable, not lost. Graders
  assert on both halves.
- Rejected: "no exception escapes" alone — provably gameable by swallowing into
  silence. | Consequences: Each source-skip and graph-exclusion task carries a
  counting/recording obligation, slightly more than a try/except.

### D-2: Grader-cannot-start is a retryable internal error, distinct from ran-and-failed  (Status: Accepted)
- Context: Today the main grader path's unguarded `Popen` turns an infra failure
  (bad cwd, missing /bin/sh, resource exhaustion) into a terminal entry crash,
  while a grader that runs and fails is a validation failure; the recheck path
  already distinguishes them (`harness.py:4049` guards with `except OSError`). |
  Decision: A grader whose subprocess fails to *start* is classified internal-error
  / retryable (infra), separate from a grader that *runs* and exits non-zero
  (validation-failed). The main path mirrors the recheck path's existing guard.
- Rejected: Treating spawn failure as validation-failed (burns a real attempt on
  an infra hiccup) or as a terminal crash (today's behavior, kills the entry). |
  Consequences: The classification seam is now load-bearing; #9's grader pins
  both directions so the distinction cannot collapse.

### D-3: Mirror the worker daemon's circuit breaker, do not invent a new one  (Status: Accepted)
- Context: The worker daemon already has the exact breaker the autopilot loop
  lacks (consecutive-failure count + backoff + bounded give-up that surfaces). |
  Decision: The autopilot breaker mirrors the worker's contract
  (`MAX_CONSECUTIVE_CYCLE_FAILURES` / `CYCLE_FAILURE_BACKOFF_SECONDS` /
  give-up-with-a-visible-non-zero/recorded-signal), so the two daemons stay
  symmetric and an operator/supervisor reads both the same way. Aligns with
  program decision D-C (auto-respawn under a crash-loop budget — the breaker is
  the per-daemon half of that budget). | Consequences: The bounded give-up must
  surface (not exit 0) so a later supervision phase (P6) can act on it.

### D-4: Gate-site containment fails closed (no land on gate error)  (Status: Accepted)
- Context: Program decision D-A keeps intentional human gates from being
  auto-bypassed; a swallowed gate-evaluation error that then proceeds to land is
  exactly an auto-bypass of the held-out gate. | Decision: Containing the gate
  call site means the pass survives AND landing is suppressed when the gate
  evaluation itself errored — fail-closed at the call site, mirroring the gate
  engine's internal discipline. | Consequences: #10's grader asserts the submit
  effect is NOT taken on a gate-evaluation error, not merely that no traceback
  escaped.

## Open Questions (accepted gaps)
None. Every criterion lowers to a `command` grader (a pytest the worker runs
out-of-band) against this repo's real verification surface.

## Next Steps
Run `/fw-plan 00065-FEATURE-containment-floor` to compile these criteria into
flywheel tasks and graders.
