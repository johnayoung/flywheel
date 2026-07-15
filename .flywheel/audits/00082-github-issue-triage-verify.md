# fw-verify record: 00082-FEATURE-github-issue-triage

**Verified:** 2026-07-15
**Spec:** `.flywheel/specs/00082-FEATURE-github-issue-triage.md`
**Tasks:** `.flywheel/tasks/active/21-github-issue-triage/` (4 tasks)
**Oracles (git-ignored scratch):** `.flywheel/verification/00082-github-issue-triage/{U1_drain_trust,U2_compile_pass,U3_pass_hygiene}/test_*.py`; execute-time adapters `<unit>/driver.py` + `<unit>/sut_real.py`
**Registrations:** `held-out/github-drain-trust.json` (U1), `held-out/triage-compile-pass.json` (U2), `held-out/triage-pass-hygiene.json` (U3)

## Routing

- AUTHOR: criteria 5+6 (U1, task `github-drain-trust`), criteria 1+2+3+4 (U2, task `triage-compile-pass`), criteria 7+9 (U3, task `triage-pass-hygiene`).
- SKIP (visible, owned by the task's own graders): criterion 8 (close-on-done), criterion 10 (CLI exit codes), criterion 12 (per-pass cap).
- ROUTE TO MANUAL: criterion 11 (daemon idle/loop continuation) — loop mechanics are wall-clock-shaped and flake-prone as a holdout; the pass-level idle-writes-nothing half is discriminated by U3's idempotency tests; the loop-continuation half rests on the task's visible daemon tests. Held-out promise for the loop half: unmet, stated plainly. Criterion 13 (authoring fidelity) — subjective; remains the plan's paired rubric screen.

## Discrimination proofs (re-run independently by the orchestrating session)

All 12 wrong references re-killed and all three correct references re-passed by this session (not agent claims); flake screen run-twice stable on every unit (U1 7/7, U2 6/6, U3 5/5, identical both runs).

| Unit | Criteria | Kills |
| --- | --- | --- |
| U1 drain-trust | #5, #6 | schedules on ready label alone (6 tests red); defaults-graders fallback fires in trust mode (6 red); stale receipt accepted after body/title edit (2 red — the metamorphic pair); exit-code-0 receipt accepted as fail-first (1 red) — 4/4 |
| U2 compile-pass | #1, #2, #3, #4 | ready flip without authoring a block/receipt (6 red); receipt stamped without consulting the executor, fabricated exit_code=1 (killed by the exit-code co-variation relation: executor 2 vs 7 must appear verbatim in the receipt); vacuous accept — ready despite executor returning 0 (1 red); cannot-compile silent skip — needs-detail with zero comments (1 red) — 4/4 |
| U3 pass-hygiene | #7, #9 | re-authors every ready issue every pass (body write on unchanged pass 2); drift never re-checked — stale receipt survives under ready label (2 red, body+title); needs-detail re-commented every pass (1 red); content hash covers body only, title edits invisible (1 red) — 4/4 |

## Null-reference kill on the real system

All three drivers RED (exit 1) against current main through the real seams, zero DRIVER-ERRORs, every failure a named ADAPTER-BIND naming the exact absent surface:

- U1: 7F — `GithubWorkSource.__init__() got an unexpected keyword argument 'require_triage_receipt'` (A1) and `_triage_receipt` module absent (A2).
- U2: 6F — `No module named 'flywheel_orchestrator._triage'` (B1).
- U3: 5F — same B1 absence through the stateful world.

## Bridging assumptions (adapter, not oracle, risks — flagged for gate-time adjudication)

Each mis-guess fails CLOSED (ADAPTER-BIND pytest.fail or DRIVER-ERROR parks the landing) and is fixed by re-pointing the operator-side adapter, never the oracle:

1. U1-A1: the trust knob is the constructor kwarg `require_triage_receipt` (pinned in the task brief). U1-A2: receipt/hash/block helpers discoverable in `flywheel_orchestrator._triage_receipt` by name vocabulary (`*hash*`, `*receipt*` + make/build/new, `*block*` + make/build/render/format); fallback embeds the pinned four-field dict under a `triage_receipt` block key. U1-A3: drain lists via `gh issue list --label <ready>` and emits `gh-<number>` task ids.
2. U2/U3-B1: engine in `flywheel_orchestrator._triage`, entry discoverable as `run_*pass*`/`*triage*pass*` function or a `*Triage*` class with `run_pass`/`run_once`/`run`. B2: seams are keyword params named by vocabulary (runner/gh, author/invoker/agent/compile, executor/exec/prove, intake_label/ready_label/needs_detail_label, log); a prompt-shaped invoker is answered with a JSON `{"goal","command"}` / `{"cannot_compile","gaps"}` guess. B3: GitHub writes shell gh-style argv translatable via `--add-label`/`--remove-label`/`comment --body`/`edit --body`; unrecognized write argv fails the bind. B4: same receipt vocabulary as A2. U3-C1: human edits preserve an embedded block verbatim.
3. The U2/U3 adapters predate the engine entirely and are PROVISIONAL: expect a gate-time re-point once `triage-compile-pass` lands and the real seam names are known (the 00074/00079 adjudication precedent). The oracles themselves are contract-complete and do not change.

## Honest limits

- This stage proves blind that discriminating oracles exist and records the proof; the execute-time gate on the real runs is the three registrations above (run by the orchestrator's held-out landing gate, `[held_out] root` is set) plus each task's own command graders.
- `triage-verb-daemon` carries no held-out registration: criterion 10/12 are visible, criterion 11's loop half is manual-routed. Its gate is the task's own graders including the committed docs-parity suite.
- A correct implementation with an unanticipated shape can read RED/ADAPTER-BIND at gate time: the landing parks (fail-closed), the strand surfaces in status, and the operator adjudicates by updating the adapter and re-driving.
- A held-out suite is a filter, not a correctness proof.

## Fences

All four task briefs in `21-github-issue-triage/` carry "Do not read or write under .flywheel/verification/" in `non_goals` (applied at verify time; `flywheel validate` green after the edit).
