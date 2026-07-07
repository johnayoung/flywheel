# fw-verify record: 00073-FEATURE-run-decision-record

**Verified:** 2026-07-07
**Spec:** `.flywheel/specs/00073-FEATURE-run-decision-record.md`
**Tasks:** `.flywheel/tasks/active/09-run-decision-record/` (6 tasks)
**Oracles (git-ignored scratch):** `.flywheel/verification/00073-run-decision-record/U*/oracle.py`, gated by each unit's `gate.py`; execute-time adapter `driver.py`
**Registrations (out-of-worktree held-out root):** `attempt-grader-history.json` (U5), `run-detail-decision-surface.json` (U1, U2, U3, U4, U6)

## Routing

- AUTHOR: criteria 1, 2, 3, 5, 6, 8 (six blind oracle units U1-U6).
- SKIP (already un-gameable): criterion 12 (operator merge gate + diff review), 13 (existing committed suites, out-of-band by construction), 14 (satisfied by this stage). Visible criteria 4, 7, 9, 10, 11 stay with the tasks' own graders.
- Blind authors: six independent fresh-context agents, each given only its fence pack (criterion + declared contract + oracle interface). No flywheel imports, nothing read under `packages/`; references pure in-memory.

## Discrimination proofs (verified independently by the operator-side run, not agent claims)

All gates re-run by the orchestrating session after authoring AND after amendments: correct reference PASSED, kills as below, flake screen stable (2 identical rounds), exit 0.

| Unit | Criterion | Form | Kills (wrong reference -> discriminating input) |
| --- | --- | --- | --- |
| U1 gate-verdict | #1 | property + content | synthetic_verdict (no gate run: no grader entries on fail scenario); empty_output_excerpt (nonce `ALPHA-OUT-nonce-77f2` absent); only_failures_recorded (pass-case record empty); outcome_not_recorded (alpha-fails vs bravo-fails byte-identical) — 4/4 |
| U2 park-output | #2 | property | reason_string_only (the named gaming move: "name exit 3" without the nonce-bearing output); drops_output; omits_kind (`held-out-gate` absent); stale_cached_record (run 2 record lacks its own output) — 4/4 |
| U3 landing-reference | #3 | metamorphic (observed-reference seam) | constant_fabricated_reference; recorded_at_submit_start (reference present while observer saw nothing land); reference_dropped; stale_first_reference (run 2 carries run 1's ref) — 4/4 |
| U4 redrive-pairing | #5 | property (append-only witnesses) | mark_redriven_without_reattempt (no `standing-verify` re-park witness); overwrite_park_in_place (original gate nonce gone); drop_redrive_outcome (no outcome witness) — 3/3 |
| U5 attempt-history | #6 | property (structural attribution) | last_attempt_only; final_receipts_duplicated; attempt_keys_off_by_one; final_outcomes_stamped_on_all — 4/4 |
| U6 telemetry-loss | #8 | metamorphic (kept vs lost pair) | telemetry_only_crash; telemetry_only_vanish; partial_on_loss; hardcoded_record — 4/4 |

## Realizability amendments (blind; declared-contract corrections only)

Initial fence packs mis-declared four system facts; affected authors were re-fenced and re-gated without seeing any implementation:

1. U1: the held-out gate evaluates at most once per run — multi-evaluation case removed.
2. U3/U6/U4: the landed reference is content-addressed and cannot be scenario-chosen — seam changed to `execute(scenario) -> {"record", "landed_reference"}` with the reference observed out-of-band (git); U3's PR-identifier positive case dropped (not hermetically realizable; covered by the task's visible tests).
3. U4: a re-driven landing does not re-run the held-out gate; a failing re-drive re-parks with kind `standing-verify` — the re-park's guaranteed witness is that distinct kind, not a grader-output nonce.
4. U5: a run's grader list is fixed across attempts (same names each attempt) — attempt witnesses are per-attempt outputs/outcomes, not disjoint names.

## Null-reference kill on the real system

`driver.py --unit U1..U6` run against current main (pre-implementation): every unit ORACLE-RED with zero DRIVER-ERRORs; U4 re-run gave a byte-identical verdict. The reds name exactly the missing behavior (no gate receipts, no park content, no landing reference, last-attempt-only grader receipts, no durable decisions), proving the adapters construct real scenarios end-to-end (real orchestrate, real GitWorktreeSubmitter merges, real gate parks, real redrive_parked_landings) and the oracles bind to the real retrieval surface (`flywheel show <task> --json --db`).

## Registration placement (sequencing constraint)

Criteria 1, 2, 3, 5, 8 span emission (tasks 1-4) plus CLI rendering (task 6), so their oracles are satisfiable only once `run-detail-decision-surface` lands — registering them on the emission tasks would falsely park conforming work. Placement:

- `attempt-grader-history` -> U5 (that task owns criterion 6 end-to-end).
- `run-detail-decision-surface` -> U1, U2, U3, U4, U6 (its prerequisites put all emission work on the base by then).
- Tasks `gate-verdict-record`, `landing-success-record`, `redrive-record`, `audit-landing-stream` land gated by their own graders plus `[submit] verify` only; any emission defect surfaces at task 6's gate and is re-drivable after a fix.

## Honest limits and findings (route upstream)

1. **Plan gap vs criterion 2 breadth:** criterion 2 covers all grader-decided parks, but the task set implements deciding-grader output only for held-out-gate parks (`standing-verify` parks keep their fixed-message detail; no task's brief covers capturing `[submit] verify` output into the park payload). U4 was amended to not depend on it. Operator decision needed: extend a task brief before execution, or accept held-out-only output for this phase and spec the remainder later.
2. **Gate-bypass observation (out of scope here):** `redrive_parked_landings` re-attempts the land without re-evaluating the held-out gate, so a gate-blocked park can land on re-drive via the strategy's own checks alone. Recorded as an observation for a future spec; the 09-phase tasks' non_goals correctly forbid changing redrive semantics.
3. U1's byte-differential assertions (pass-vs-fail, swap, no-gate-vs-never) are load-bearing at authoring time (they killed `outcome_not_recorded`) but vacuous at execute time (real records always differ in run ids/timestamps); execute-time enforcement rests on the content assertions. The no-gate verdict's literal spelling remains unpinned — covered differentially, never by an invented literal.
4. In-process `orchestrate` may write no per-run JSONL, in which case U6's kept-vs-lost pair degenerates (both read durable state); the unit still enforces criterion 8's substance (decisions retrievable without telemetry) and cannot false-park.
5. The driver couples to public seams (`orchestrate`, `GitWorktreeSubmitter`, `redrive_parked_landings`, `SubmitRequest`, `flywheel show --json --db`). A signature change would surface as DRIVER-ERROR (exit 3) at gate time — fail-closed, distinguishable from an oracle red.
6. This stage proves blind that discriminating oracles exist and records that proof; the registrations route them to the orchestrator's execute-time held-out gate (active: `[held_out] root` is configured). The durable regression guard remains the tasks' own tests in the normal suite.

## Execution addendum (2026-07-07, during the phase run)

1. **Gate bypass confirmed live.** `attempt-grader-history` was gate-FAILED and parked at landing, then FF-merged to main at 13:41:15 by the landing re-driver, which re-attempts `strategy.submit` without re-evaluating the held-out gate. Finding 2 above is no longer theoretical; gate verdicts are advisory once a park is redriven. Route to /fw-improve.
2. **U5 false park, post-mortem.** The gate FAIL was not the work: the landed change conformed to criterion 6 (attempt-keyed `grader_results` with identity + outcome). Two oracle-side defects caused the red: (a) my amendment made U5 demand grader OUTPUT text in the record — unstated by the criterion (the exact hidden-oracle defect the protocol warns about); (b) the driver's per-grader invocation counters drifted because the harness ends an attempt at its first grader failure. Fixes: third blind amendment (no output witnesses; prefix semantics — an attempt's verdict set is a prefix of the grader list ending at its first failure), driver rewritten to a leader-ticked shared attempt counter. Post-fix: U5 gate 4/4 kills, correct passes, stable; driver GREEN twice against the landed main — the amended oracle certified a real conforming implementation while still killing all four wrong references.
3. The stale `stranded` status line for that run persists by design: it landed before the `Landed` domain event existed (eb585b9), so no positive witness can clear it retroactively.

## Fences to apply (operator-owned task edit)

Add to `context.non_goals` of all six tasks in `.flywheel/tasks/active/09-run-decision-record/`:
"Do not read or write under .flywheel/verification/"
