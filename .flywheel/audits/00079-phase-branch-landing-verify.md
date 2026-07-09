# fw-verify record: 00079-FEATURE-phase-branch-landing

**Verified:** 2026-07-09
**Spec:** `.flywheel/specs/00079-FEATURE-phase-branch-landing.md`
**Tasks:** `.flywheel/tasks/active/20-phase-branch-landing/` (4 tasks)
**Oracles (git-ignored scratch):** `.flywheel/verification/00079-phase-branch-landing/{P1_phase_strategy_landing,P2_phase_pr_completion,P3_archival_on_merge,P4_prereq_reachability}/test_*.py`; execute-time adapters `<unit>/driver.py` + `<unit>/sut_real.py`
**Registrations:** `phase-strategy-landing.json` (P1), `phase-pr-at-completion.json` (P2), `phase-archival-on-merge.json` (P3), `phase-prereq-reachability-hold.json` (P4)

## Routing

- AUTHOR: criteria 1+2+9 (P1), criterion 3 (P2), criteria 4+5+8 (P3), criterion 7 (P4).
- SKIP (visible, owned by the task's own graders): criterion 6 (merge-method-mismatch surface, task `phase-archival-on-merge`'s visible tests).
- SKIP (already un-gameable at operator scope): criterion 10 (verification-surface DoD — enforced by `[phase] verify` / `[submit] verify` plus diff review).

## Discrimination proofs (re-run independently by the orchestrating session)

All 16 wrong references re-killed and all four correct references re-passed by this session (not agent claims); flake screen run-twice stable on every unit.

| Unit | Criteria | Kills |
| --- | --- | --- |
| P1 phase-strategy-landing | #1, #2, #9 | rename-only strategy still landing on base; standing verify wired to the merge path only; protected paths wired to the merge path only; phase-branch bookkeeping leaking into strategy=merge — 4/4 |
| P2 phase-pr-completion | #3 | create-on-every-sweep (duplicates); decorative body (killed twice: missing receipt names AND a metamorphic verdict-flip leaving the body unchanged); phase gate run in the operator checkout (killed from both directions via a marker-file verify pair); per-task PRs relabeled as the phase unit — 4/4 |
| P3 archival-on-merge | #4, #5, #8 | feature-absent all-DONE archiver; self-merging sweep (advances the true base itself); remote-merged-claim truster (squash case); `.loop-base` mis-content (tip SHA instead of the recorded base) — 4/4 |
| P4 prereq-reachability | #7 | DONE-suffices (ignores reachability); sticky-hold (never releases); invisible-hold (no reason naming the phase); over-blocking (holds same-phase chains) — 4/4 |

## Null-reference kill on the real system

All four drivers RED (exit 1) against current main through the real seams, with zero DRIVER-ERRORs, and — the strongest adapter evidence — every oracle test that pins TODAY's shipped behavior passes inside the red runs:

- P1: 5F/2P — every `strategy="phase"` test fails at the real seam (`PolicyError: submit.strategy must be one of ('merge', 'pr'), got 'phase'`); the merge-strategy tests pass end-to-end (real FF land, real `parked:standing-verify`).
- P2: 3F/2P — `gh_log` stays empty (no PR machinery on main); the no-PR tests pass.
- P3: 3F/1P — main archives on all-DONE regardless of ancestry (unmerged, remote-claim, and CLI-caller tests all red); the ancestor-archives-with-`.loop-base` test passes (shipped behavior).
- P4: 3F/3P — an unreachable-phase DONE prerequisite is silently claimable and holds are empty on main; same-phase stacking, the reachable case, and ordinary prerequisite waiting pass (shipped selection semantics).

## Bridging assumptions (adapter, not oracle, risks — flagged for gate-time adjudication)

These adapters bind to surfaces the 00079 implementation will create; each mis-guess fails CLOSED (oracle RED or DRIVER-ERROR parks the landing) and is fixed by updating the operator-side adapter, never the oracle:

1. P1 resolves the strategy builder by introspection over a bound context vocabulary (policy/paths/store/log/verify/etc.); a new required builder parameter outside it reds the oracle until added. The landed target is pinned to `flywheel/phase/<phase>` (the spec's declared name).
2. P2 assumes the phase-PR sweep is reachable through `worker.archive_phases` (or params it grows) and that PR operations shell a `gh` executable on PATH (the shim's observation channel); a separate entry point or a non-gh PR channel is invisible until the adapter is re-pointed. Receipts are seeded as attempt-1 `grader_results` rows plus `HeldOutGateEvaluated` events in the run ledger.
3. P3 reads the daemon-path report only from the sweep's `log` callable (CLI path captures stdout+stderr); seeds `Landed` witnesses to satisfy the in-flight 00077 landed predicate.
4. P4 assumes the reachability gate is observable through `select_next_task` and that hold reasons reach the plain-text `flywheel status` surface on or under the task's row; a claim-path-only gate or JSON-only surface stays red until the binding is re-pointed. Holds are never synthesized — empty on main by design.

## Honest limits

- This stage proves blind that discriminating oracles exist and records the proof; the execute-time gate on the real runs is the registrations above plus each task's own command graders.
- Because these adapters predate the implementation, a correct implementation with an unanticipated shape can read RED/DRIVER-ERROR at gate time: the landing parks (fail-closed), the strand surfaces in status, and the operator adjudicates by updating the adapter and re-driving — the 00074 precedent.
- P1's merge-compat facet (criterion 9) is graded live at gate time by the same oracle run that grades the phase facets; it already passes on main.

## Fences

All four task briefs in `20-phase-branch-landing/` carry "Do not read or write under .flywheel/verification/" in `non_goals` (applied at plan time).
