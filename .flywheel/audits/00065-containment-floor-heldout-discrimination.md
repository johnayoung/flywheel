# Held-out discrimination proofs — spec 00065 Phase 1 (containment floor)

Stage: fw-verify (blind-author held-out oracles, BEFORE execute).
Date: 2026-06-30. Phase: ACTIVE (registered oracles gate the next worker run).

Each oracle was authored blind from the declared contract (no implementation
in view; the implementation does not exist yet), then run by the REAL runner
(`uv run pytest`) against one synthesized CORRECT reference and 3-4 synthesized
PLAUSIBLE-WRONG references. Admission requires: CORRECT passes, at least one
WRONG dies on a discriminating input that reaches the assertion, and a
run-twice flake screen yields an identical verdict. Each oracle also imports
and runs against the real (currently un-contained) module and FAILS today —
confirming it grades the real contract, not a tautology.

Scratch (git-ignored): `.flywheel/verification/00065-containment-floor/`.
References are throwaway scratch and are NOT committed and NOT shown to the
implementing agent.

---

## Criterion #4 — directory-source-skip-bad-item (task: directory-source-skip-bad-item)

- Oracle: `test_directory_source_skip_holdout.py` (property / metamorphic:
  returned ids == exactly the valid ids in walk order, regardless of where the
  one bad file sits; bad file placed in the MIDDLE is the discriminating input).
- Form: property (filter-preserves-valid, order-preserving).
- Discriminating input: a phase dir with files `a-first.json` (valid),
  `m-broken.json` (unparseable JSON), `z-last.json` (valid).
- Discrimination result:
  - CORRECT (skip-and-continue) -> PASS.
  - WRONG `ref_wrong_abort` (raise on first bad, today's behavior) -> KILLED
    (JSONDecodeError crash escapes; `ids == ["a-first","z-last"]` never reached).
  - WRONG `ref_wrong_empty` (swallow into []) -> KILLED (`[] != ["a-first","z-last"]`).
  - WRONG `ref_wrong_break` (break instead of continue) -> KILLED
    (`["a-first"] != ["a-first","z-last"]`, drops the trailing valid `z-last`).
- Flake screen: run-twice against CORRECT -> `2 passed` both runs (stable).
- Real-module run (no FW_REF): FAILS today (current `load_active_tasks` aborts
  on the first bad file) — confirms the oracle grades the real contract.
- Note: `DirectoryWorkSource.__init__` exposes no log seam, so this oracle
  asserts the survivors-preserved half (the spec's primary "defends against":
  empty/partial list called handled). The recorded-count half stays the task's
  own command grader.

## Criterion #5 — github-source-skip-bad-item (task: github-source-skip-bad-item)

- Oracle: `test_github_source_skip_holdout.py` (property: returned ids ==
  exactly the valid `gh-<number>` ids AND the skip is recorded via the `log`
  seam — the D-1 contain-AND-record obligation).
- Form: property (filter-preserves-valid) + side-effect (recorded skip).
- Discriminating input: payloads `[valid#1, malformed(no integer 'number'),
  valid#3]`, injected via the `runner` seam as canned `gh` JSON stdout; `log`
  captured.
- Discrimination result:
  - CORRECT (skip + log) -> PASS.
  - WRONG `ref_wrong_abort` (abort whole listing) -> KILLED.
  - WRONG `ref_wrong_empty` (swallow into []) -> KILLED.
  - WRONG `ref_wrong_silent` (return survivors but log NOTHING) -> KILLED on
    `assert len(log_lines) == 1` (got 0) — it PASSES the survivors assertion
    `ids == ["gh-1","gh-3"]`, proving the discriminator reaches the
    recorded-half assertion. This is the D-1 "swallow into silence" fake.
  - WRONG `ref_wrong_break` (break drops trailing valid) -> KILLED.
- Flake screen: run-twice against CORRECT -> `2 passed` both runs (stable).
- Real-module run (no FW_REF): FAILS today (current `_compile_issue` path
  aborts on the first bad payload).

## Criterion #8 — workgraph-isolate-invalid-task (task: workgraph-isolate-invalid-task)

- Oracle: `test_workgraph_isolate_holdout.py` (property: build does not raise on
  a structural defect; valid independent tasks present in `ready_set`; offender
  absent from it; recorded reason text NAMES the offender). Self-dependency is
  the chosen single-task defect (unambiguous offender).
- Form: property (both halves: survivors-schedulable AND recorded-reason-names-offender).
- Discriminating input: items `[(valid-alpha, ()), (loops-on-self,
  (loops-on-self,)), (valid-beta, ())]`.
- Discrimination result:
  - CORRECT (isolate + record reason naming offender + keep survivors) -> PASS.
  - WRONG `ref_wrong_raise` (raise on defect, today's behavior) -> KILLED.
  - WRONG `ref_wrong_drop_all` (empty ready set) -> KILLED (survivors absent).
  - WRONG `ref_wrong_silent` (exclude offender, record no reason) -> KILLED on
    `assert OFFENDER in recorded` (recorded == '') — PASSES the survivors half,
    proving the discriminator reaches the recorded-reason assertion.
  - WRONG `ref_wrong_unnamed_reason` (records a reason but does not name the
    offender) -> KILLED on `assert OFFENDER in recorded`
    (recorded == 'a task was structurally invalid').
- Flake screen: run-twice against CORRECT -> `1 passed` both runs (stable).
- Real-module run (no FW_REF): FAILS today with `WorkGraphValidationError`
  (current `WorkGraph.__init__` raises on self-dependency).
- Contract-pinning caveat: the recorded-reason RECORD TYPE is left open by the
  spec ("a GraphValidationIssue-style record or equivalent"). The oracle reads
  the recorded surface defensively (stringifies `result.issues`) and asserts
  the OBSERVABLE "a reason naming the offender", not a specific class. If the
  implementer records exclusions on a different attribute than `result.issues`,
  the registered held-out gate will need the implementer to surface them on
  `GraphValidationResult.issues` (the declared idiom the task says to "extend").
  This is the one place the contract is loose; see routing notes.

---

## Honesty statement

These oracles prove BLIND, before execute, that a discriminating held-out test
EXISTS for criteria #4, #5, #8 and record the kill-and-pass evidence. They do
NOT by themselves prove the agent's real run is correct: they gate it only when
the orchestrator runs the registered `<task_id>.json` held-out grader from
outside the worktree. A held-out suite is a filter, not a correctness proof; a
finite oracle can still be slipped past. Criteria #1,#2,#3,#6,#7,#9,#10 were
triaged out of the AUTHOR bucket (see routing table) and stay gated by each
task's own command graders plus `scripts/check.sh`.
