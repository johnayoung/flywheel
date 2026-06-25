# fw-verify discrimination proofs — 00050 execute-time held-out landing gate

Date: 2026-06-25. Stage: fw-verify (held-out oracle authoring), run POST-execute
(spec 00050 shipped to `main` at commits `fd523cb` engine / `d9ae08f` landing
integration). This audit adds the blind held-out grade for a feature whose own
task graders were agent-authored — exactly the loop fw-verify exists to close.

Each oracle was authored BLIND by an independent subagent from a fenced contract
(the criterion + observable `gate(graders, work_dir, agent_passed) -> verdict`
seam; NO implementation body, NO agent tests, NO reference solution), then proven
to DISCRIMINATE (passes a synthesized correct reference, kills >=1 synthesized
plausible-wrong reference). Oracles live git-ignored under
`.flywheel/verification/00050/<unit>/`; only this proof is durable. Each oracle
was also REAL-GRADED: the `gate` seam re-pointed (via an adapter) at the shipped
`evaluate_held_out_gate` + `FilesystemHeldOutGraderSource`.

Legend: ADMITTED = discrimination-proven + flake-stable (2 identical runs).
REAL-GRADE = oracle run against the shipped implementation.

## Routing (11 criteria)

- AUTHOR held-out oracle: #2,#4,#6,#7 (verdict matrix); #5 (fail-closed). 2 units.
- SKIP / real-grade probe: #3 (agent-isolation — structural; covered by a direct
  probe below), #8 (gate-before-submit-under-lease — integration sequencing,
  graded by the committed orchestrator suite), #9 (visible tier).
- SKIP (un-gameable / out-of-band): #1 (success-path — entailed by #7 + suite),
  #10 (suites — `command`), #11 (no-core-change — `command` + git-diff;
  independently confirmed: `git diff` touches no `flywheel-core/src`).
- Routed to manual: none.

## Units

- **Unit A** `gate-verdict` — #2,#4,#6,#7 (the verdict matrix: no-registration ->
  NO_GATE/no-block; all pass -> PASS/no-block; any fail -> FAIL/block; the agent's
  self-report never changes the verdict; the three outcomes are distinct). ADMITTED.
  - killed (4/4): collapse_no_gate_into_pass (conflates an opted-out task with a
    passing one — #6/#7); honors_agent_passed (a DONE agent skips the gate — #4);
    inverts_blocks; failing_grader_still_lands (#2). correct ref PASSED.
  - REAL-GRADE: **PASS** against `evaluate_held_out_gate`. Note the shipped engine
    enforces #4 structurally — it is never passed the agent's terminal status and
    so cannot consult it; the adapter confirms an `agent_passed=True` scenario with
    a failing held-out grader still yields FAIL/block.

- **Unit B** `fail-closed` — #5 (a registered-but-unrunnable held-out grader ->
  FAIL/block, never NO_GATE and never PASS; distinct from the no-registration
  NO_GATE case). ADMITTED. **The headline anti-hack** (make the hidden check
  unrunnable so the work lands ungated).
  - killed (4/4): unrunnable_is_no_gate (fail-OPEN — the core bug, conflates a
    broken registration with no registration); unrunnable_passes (silently passes
    a broken check); blocks_only_if_agent_failed (a DONE agent slips through a
    broken gate); good_masks_broken (a passing grader masks a broken one). correct
    ref PASSED.
  - REAL-GRADE: **PASS**. A malformed `<root>/t.json` registration drives the real
    `FilesystemHeldOutGraderSource` to raise `HeldOutGraderError`, which the engine
    maps to a FAIL verdict (`blocks_landing` True) — fail-closed confirmed against
    shipped code.

## #3 isolation probe (direct real-grade)

Held-out grading is only meaningful if the agent cannot reach the check. A direct
probe against shipped types confirmed: the agent-facing `Task` carries no held-out
grader (`task.graders == []`); the held-out registration file lives under the
source `root`, NOT anywhere under the committed working tree (`work.rglob("*.json")`
is empty); and `evaluate_held_out_gate` still loads and runs it against the
committed tree (PASS). So the held-out grader is a side channel the orchestrator
reads, absent from the agent's Task and worktree (D-2 / #3), yet still authoritative.

## Summary

- Authored: 2 blind held-out oracles, both ADMITTED (all 4 wrong refs killed each,
  flake-stable across 2 runs). No equivalent-mutant survivors this round.
- Against-real-impl held-out grade: **2/2 PASSED** against shipped
  `evaluate_held_out_gate` + `FilesystemHeldOutGraderSource`, plus the #3 isolation
  probe PASS.
- Notable: the shipped engine enforces D-4 (agent-report-never-authoritative)
  *structurally* — it never receives the agent's status — which is stronger than
  the criterion required.
- Honest limits: this stage proves blind that a discriminating oracle EXISTS and
  that the shipped code passes it; the oracles are git-ignored and do NOT gate
  future agent runs. The durable regression guard stays the tasks' own command
  graders (`uv run pytest packages/flywheel-orchestrator/tests/` +
  `packages/flywheel-worktree/tests/`) and the committed suites. Recursive note:
  00050 IS the machinery that could eventually run held-out oracles as an
  execute-time gate — but wiring fw-verify's git-ignored oracles into operator
  held-out registrations is itself out of scope here.
