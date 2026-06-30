# fw-verify discrimination proofs -- 00067 transient-fault resilience (Phase 3)

Date: 2026-06-30. Stage: fw-verify (held-out oracle authoring), run PRE-execute
(spec 00067, Phase 3 tasks STAGED under `.flywheel/tasks/staged/03-transient-resilience/`,
not yet promoted). No implementation exists yet, so authoring is SOUND directly
from the declared contract: there is no implementation body or agent test to leak,
and every oracle asserts ONLY the spec's declared relations.

Each oracle was authored BLIND from the DECLARED contract (spec 00067 criterion
text + Decisions Log D-1..D-4 + Background) and proven to DISCRIMINATE by the
INVERTED mutation gate: synthesize ONE correct reference + 2-5 plausible-wrong
references, run the oracle against each, require the correct ref PASS and >=1
wrong ref KILL. Oracles + gates live git-ignored under
`.flywheel/verification/00067-transient-fault-resilience/<unit>/`; only THIS proof
is durable. The synthesized references are throwaway scratch and never ship.

Legend: ADMITTED = correct-ref passes + >=1 wrong-ref killed + flake-stable
(run-twice identical). KILL = oracle goes red on a seeded-wrong reference.

## Routing (10 criteria)

- AUTHOR held-out oracle (discrimination-proven this run):
  - #9 classification matrix (composition holdout) -- the TRANSIENT/PERMANENT
    partition relation. Owning task `transient-classifier-backoff`.
  - #3 bounded backoff -- monotonic-non-decreasing AND capped property. Owning
    task `transient-classifier-backoff`.
  - #1 separate transient budget -- DONE-at-max_retries=0 vs genuine-failure
    metamorphic contrast. Owning task `api-rate-limit-retry`.
- SKIP -> ride the task's own command graders (un-gameable structural/state, or a
  variant of an AUTHORED relation already pinned):
  - #2 rate_limit_exhausted -- bounded attempt count = N+1 + TRANSIENT label;
    structural count + enum-value check, deterministic, graded by the named
    pytest target. (Its TRANSIENT label is the same partition #9 grades.)
  - #4 sqlite locked_retry -- "operation returns its real result"; a concrete
    state/value check the named pytest target grades deterministically.
  - #5 sqlite locked_classified -- a TRANSIENT enum-value assertion; same
    partition #9's matrix already discriminates (locked -> TRANSIENT).
  - #6 postgres pool_acquire_bounded -- wall-time-bounded + TRANSIENT label,
    container-gated; structural/timing check the named pytest target grades.
  - #7 schema_mismatch_permanent_stop -- strike-count == 1 + PERMANENT label;
    a concrete count assertion (1 vs MAX_CONSECUTIVE_CYCLE_FAILURES) the named
    orchestrator test grades deterministically. PERMANENT label = #9's partition.
  - #8 transient_not_breaker (visible) -- consecutive_failures == 0; concrete
    counter-state check, graded visibly by the named worker test.
  - #10 two-worker deflake (visible) -- determinism + byte-for-byte assertion
    preservation; a test-determinism / assertion-integrity property, graded by
    re-running the named test + the assertion-presence requirement. Not a
    behavior an injected-fault oracle can grade.
- Routed to MANUAL: none.
- Returned UPSTREAM as under-specified (REGISTRATION half only, see below): the
  stable importable INTERFACE for #1/#3/#9 (classifier symbol + enum member
  names, backoff-helper signature, transient-budget config field) -- needed to
  bind an execute-time out-of-worktree gate against the committed tree.

Triage rationale: #2/#4/#5/#6/#7/#8 each pin a CONCRETE value or enum member that
the named `command` grader checks deterministically and the agent cannot fake
without failing that check; their only behavioral content (the TRANSIENT /
PERMANENT label) is exactly what #9's composition holdout discriminates as a
RELATION. Authoring a separate per-case held-out oracle for each would add brittle
files that grade nothing #9 + the structural checks do not already grade. #1/#3/#9
are the genuine behavior relations where a plausible-wrong impl is cheap and
high-stakes (mis-classify a permanent fault as transient -> retry forever; consume
the validation budget; fabricate success; flat/uncapped backoff), so they are the
oracle-worthy author candidates.

## Admitted oracles (blind, discrimination-proven, flake-stable)

### #9 -- classification matrix (composition holdout)
`.flywheel/verification/00067-transient-fault-resilience/c9-classifier-matrix/`
- oracle: `oracle_test.py::check_classification_matrix`
- Form: METAMORPHIC partition relation. Asserts (a) all four in-scope transient
  faults map to ONE class; (b) schema-mismatch maps to the OTHER class; (c) the
  two classes are DISTINCT; (d) D-3: a version conflict is NOT in the transient
  class. No hardcoded enum value -- only the relation between buckets.
- Discrimination proof (`refs_and_gate.py`, run twice, identical):
  - CORRECT (schema->PERMANENT, four faults->TRANSIENT, vc->not-transient): PASS.
  - WRONG-1 blanket-everything-TRANSIENT: KILLED (classes not distinct -- the
    cheapest fake that retries a hopeless schema mismatch forever).
  - WRONG-2 schema-mismatch-bucketed-TRANSIENT: KILLED (classes not distinct).
  - WRONG-3 dropped-PG-connection-bucketed-PERMANENT: KILLED (transient faults
    do not share one class -- the inconsistent-union defect #9 targets).
  - WRONG-4 version-conflict-bucketed-TRANSIENT: KILLED (D-3 violation).
  - Killed 4/4 wrong refs; correct passed. Flake-screen run-twice: STABLE.
- ADMITTED.

### #3 -- bounded backoff (monotonic + capped)
`.flywheel/verification/00067-transient-fault-resilience/c3-backoff-bounded/`
- oracle: `oracle_test.py::check_backoff_bounded`
- Form: PROPERTY over the captured wait sequence (injected sleep recorder).
  Asserts (a) >=2 waits captured; (b) non-negative; (c) monotonic-non-decreasing;
  (d) max(waits) > 0 (not flat-zero); (e) every wait <= configured cap.
- Discrimination proof (run twice, identical):
  - CORRECT capped-exponential: PASS.
  - WRONG-1 constant sleep(0): KILLED (max(waits) > 0 -- no real backoff).
  - WRONG-3 uncapped exponential: KILLED (a wait exceeds cap -- DoS/hung daemon).
  - WRONG-4 decreasing waits: KILLED (not monotonic-non-decreasing).
  - WRONG-5 no-sleep-at-all: KILLED (<2 waits captured).
  - WRONG-2 constant NON-zero under cap: PASSES -- HONEST EQUIVALENT VARIANT. The
    declared contract (criterion #3 + D-4) requires "no shorter than the first"
    (non-decreasing, which a flat constant satisfies) and "no greater than cap";
    it does NOT require STRICT growth. A flat nonzero capped schedule is permitted
    by the declared contract, so this is an unkillable equivalent, NOT a weak
    oracle. Per the gate's honest tolerance, the kill of 4 genuine wrong refs is
    the proof; WRONG-2 is not weakened to force a kill (that would game the gate
    and demand unstated strict-growth behavior).
  - Killed 4/5 wrong refs; correct passed. Flake-screen run-twice: STABLE.
- ADMITTED.

### #1 -- separate transient budget (metamorphic contrast)
`.flywheel/verification/00067-transient-fault-resilience/c1-separate-budget/`
- oracle: `oracle_test.py::check_separate_budget`
- Form: METAMORPHIC contrast between two drives of the same driver at
  max_retries=0. Asserts (A) [rate-limit, rate-limit, valid-pass] -> terminal
  DONE (two transient hiccups absorbed by a SEPARATE budget, not the exhausted
  validation budget; Background pins ">=2 absorbed"); (B) a genuine
  MissingEnvelope with NO rate-limit signal -> terminal NOT DONE (real failures
  still consume the validation budget). The A-vs-B contrast is the discriminator.
- Discrimination proof (run twice, identical):
  - CORRECT separate-budget driver: PASS.
  - WRONG-1 raise-max_retries / no-separate-budget (the rejected D-1 fake):
    KILLED on A (max_retries=0 -> first non-pass step fails, never reaches DONE).
  - WRONG-2 fabricate-success (return DONE always): KILLED on B (a genuine
    failure reaches DONE -- swallowed real failure).
  - WRONG-3 today's no-transient-handling (429 -> MissingEnvelope -> budget):
    KILLED on A.
  - WRONG-4 separate-budget-defaulted-to-1 (only ONE rate-limit absorbed):
    KILLED on A (second rate-limit fails; violates Background's ">=2 absorbed").
  - Killed 4/4 wrong refs; correct passed. Flake-screen run-twice: STABLE.
- ADMITTED.

## Registration -- BLOCKED on one upstream pin (NOT shipped, to avoid a false gate)

The contract requires registering each admitted oracle at
`<held_out-root>/<task_id>.json` as a `command` grader that invokes the oracle BY
ABSOLUTE PATH with cwd = the agent's committed tree. `[held_out] root` IS
configured (`.flywheel/verification/held-out`), so the channel exists.

It is NOT writable for #1/#3/#9 this run WITHOUT inventing an interface the spec
does not declare. The three oracles are interface-PARAMETRIC (they take
`classify` / `run_backoff` / `drive` callables): they prove a discriminating
oracle EXISTS, but to bind the agent's REAL committed tree the registration needs
a STABLE IMPORTABLE entry point, and the spec pins NONE of:
  - the classifier symbol + its two enum member names (TRANSIENT / PERMANENT);
  - the bounded-backoff helper's signature (how to drive it with an injected
    sleep/clock and read the wait sequence);
  - the transient-retry-budget config field name + a default >= 2 (Background
    pins ">=2 absorbed" behaviorally, but not the field that carries it).

Authoring a registration against an INVENTED adapter (e.g. a
`.flywheel/heldout_adapters/transient.py` the spec never asks for) would be the
inverse defect the contract forbids: a hidden oracle demanding behavior the
declared contract never stated, which would fail-closed against every CORRECT
implementation (a false gate). So no registration JSON is shipped for #1/#3/#9
this run. The discrimination proofs above stand as the durable artifact; the
execute-time gate remains the tasks' own named `command` graders (the durable,
CI-run regression guard `/fw-plan` placed), which is the same posture every prior
00067-adjacent fw-verify run landed.

To activate the out-of-worktree held-out gate for #1/#3/#9, the spec must pin the
stable interface above (route to /fw-spec, then re-run /fw-plan + /fw-verify). The
oracles here are then registrable verbatim against the pinned symbols.

## Honest limits

- This stage proves BLIND that a discriminating oracle EXISTS for #1/#3/#9 and
  records that proof; it does NOT by itself gate the agent's real run. With the
  registration blocked (above), the execute-time gate is the tasks' own command
  graders + the tests the implementing agent writes into the normal suite.
- #3 carries one honest unkillable equivalent variant (flat nonzero capped) the
  declared contract permits; it was NOT papered over.
- A held-out suite is a filter, not a correctness proof: a finite oracle can still
  be slipped past.
