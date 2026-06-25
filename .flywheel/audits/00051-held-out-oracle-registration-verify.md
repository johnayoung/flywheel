# fw-verify discrimination proofs — 00051 held-out oracle registration (close the loop)

Date: 2026-06-25. Stage: fw-verify (held-out oracle authoring), run POST-execute
(spec 00051 shipped to `main` at commits `4494278` activation / `b15ff79`
registration shape / `000d6fe` fw-verify template recursion). 00051 is the
natural recursion the prior fw-verify runs (foundation, 00049, 00050) all flagged
as missing: it activates 00050's dormant execute-time gate from committed
`[held_out] root` config and routes an admitted blind oracle into that gate. This
audit blind-grades that loop-closing feature.

Each oracle was authored BLIND by an independent subagent from a fenced contract
(the criterion + the SHIPPED 00050 gate API as the observable surface; NO 00051
implementation body — the new `build_held_out_source` / `build_oracle_registration`
/ `write_oracle_registration` code and the `_policy` held-out additions were
explicitly fenced out), then proven to DISCRIMINATE. Oracles live git-ignored
under `.flywheel/verification/00051/<unit>/`; only this proof is durable. Every
oracle was REAL-GRADED against the shipped implementation (each blind author ran
its test against the actual shipped symbols), and B/C additionally against a
concrete MUTANT (`.flywheel/verification/00051/mutants_check.py`) to make the kill
concrete rather than reasoned.

Legend: ADMITTED = discrimination-proven + flake-stable (re-run identical).
REAL-GRADE = oracle run against the shipped implementation. KILL = oracle goes
red against a deliberately-wrong implementation while green against shipped.

## Routing (10 criteria)

- AUTHOR held-out oracle:
  - A — #5,#6 (registered oracle reproduces discrimination THROUGH the gate;
    oracle by absolute path, cwd = committed tree).
  - B — #4 (registration round-trips through 00050's source AND fails closed on
    malformed / non-command / empty).
  - C — #3 (worker builds a source only when `[held_out] root` set; relative root
    resolves against repo_root, not cwd) + the activation half of #2.
- SKIP / real-grade probe:
  - #2 (inert default — structural; shipped `build_held_out_source` returns None
    when policy is None / `[held_out]` absent; asserted in `test_worker.py:860-870`
    and by C's `test_absent_held_out_table_yields_no_source`).
  - #7 (worktree isolation — the committed-pointer / git-ignored-payload property;
    `.flywheel/verification/` and the conventional held-out root are git-ignored,
    so a fresh git worktree never materializes them; `git check-ignore` confirmed).
  - #8 (fw-verify template recursion — text/render check; probed via the shipped
    `render_skill('fw-verify', SkillRenderSettings())`).
  - #1's land/block END-TO-END through the worker drive — its authoritative
    verdict (gate PASS/FAIL) is graded blind by A; activation that feeds it by C;
    the worker-drive land/block composition is the shipped 00050 landing
    integration (already fw-verified) + the green orchestrator/worktree suites.
- SKIP (un-gameable / out-of-band): #9 (suites — `command`), #10 (no-core-change —
  independently confirmed: `git diff d8df260..HEAD` touches no
  `packages/flywheel-core/src/flywheel_core/`).
- Routed to manual: none.

## Admitted oracles (blind, discrimination-proven, real-graded)

### A — discrimination reproduced through the gate (#5, #6)
`.flywheel/verification/00051/A-discrimination/test_gate_discrimination.py`
- Form: behavioral, through the shipped `evaluate_held_out_gate` +
  `FilesystemHeldOutGraderSource`. A hand-written `<root>/<task_id>.json`
  registration runs an out-of-tree oracle (absolute path) that imports `target`
  from cwd and asserts `widget(3)==6 and widget(0)==0`.
- Discrimination proof (kill-and-pass): correct tree (`n*2`) -> `GateOutcome.PASS`;
  three plausible-wrong committed trees killed -> `GateOutcome.FAIL`:
  - off-by-one (`n*2+1`): killed by both `widget(3)` and `widget(0)`.
  - identity (`n`): killed by `widget(3)==6` (returns 3).
  - constant (`6`): passes `widget(3)` but killed by `widget(0)==0` (returns 6) —
    why BOTH inputs are needed.
- External-oracle guard: asserts the oracle path is absolute and not under the
  committed tree (defends the stale-tree / wrong-cwd reward-hack).
- REAL-GRADE: PASS/FAIL outcomes are the shipped gate's, not vacuous asserts.
- ADMITTED. `5 passed`.

### B — registration round-trip + fail-closed (#4)
`.flywheel/verification/00051/B-roundtrip-failclosed/test_registration_roundtrip.py`
- Form: contract over the shipped `FilesystemHeldOutGraderSource.graders_for`.
  Pins: valid command registration -> non-empty list with the exact `run` string
  (true round-trip, not "didn't crash"); missing file -> `None`; invalid JSON ->
  `HeldOutGraderError`; rubric-only entry -> `HeldOutGraderError` (command-only
  rule); empty registration -> `HeldOutGraderError`.
- REAL-GRADE: `6 passed` against shipped source.
- KILL (concrete, `mutants_check.py`): a fail-OPEN source subclass that swallows
  `HeldOutGraderError` and returns None -> `shipped_raises=True mutant_raises=False
  -> KILL`. The malformed-raises assertion catches the dangerous fail-open
  regression (bad file silently read as "no gate" / pass).
- ADMITTED.

### C — repo-root resolution + activation (#3, #2 activation half)
`.flywheel/verification/00051/C-reporoot/test_repo_root_resolution.py`
- Form: contract over the shipped `build_held_out_source(policy, repo_root)`
  (builder discovered BLIND via `inspect.signature`, body never read). Relative
  `[held_out] root` resolves to `<repo_root>/<rel>` while running from a different
  cwd; absent `[held_out]` -> `None`.
- REAL-GRADE: `2 passed` against shipped builder.
- KILL (concrete, `mutants_check.py`): a builder resolving against `Path.cwd()` ->
  `shipped_matches_repo_root=True mutant_matches_repo_root=False -> KILL`. The
  different-cwd assertion catches the silent-miss regression (registrations under
  the wrong directory, tasks land ungated). The inert-default assertion catches a
  dormant-key / silent-default-gating builder.
- ADMITTED.

## Real-grade probes (shipped code, criteria routed to SKIP/probe)

- #2 inert default: `worker.build_held_out_source(None, ...)` and a policy without
  `[held_out] root` both return `None` (`test_worker.py:860-870`; C's absent-table
  test). Confirmed.
- #7 isolation: `git check-ignore .flywheel/verification/...` confirms the payload
  is git-ignored; a committed `[held_out] root` pointer with a git-ignored payload
  never materializes the registration or oracle into a fresh agent worktree.
  Confirmed.
- #8 template recursion: `render_skill('fw-verify', SkillRenderSettings())` ->
  no leftover `__FW_` token; rendered text instructs writing the
  `<root>/<task_id>.json` registration at the held-out root keyed by task id AND
  retains the fence against committing the oracle into the tracked repo / wiring
  it as an in-repo grader, distinguishing the sanctioned out-of-worktree channel.
  Confirmed.
- #10 no-core-change: `git diff d8df260..HEAD` touches no
  `packages/flywheel-core/src/flywheel_core/`. Confirmed.

## Aggregate

3 blind oracles authored (A/B/C), all ADMITTED. Real-graded against shipped code:
13/13 oracle assertions pass; A is full kill-and-pass (3 wrong references killed);
B and C each KILL a concrete mutant (fail-open, cwd-resolution). 4 probe criteria
(#2/#7/#8/#10) real-graded green. Full suite green on integrated main: 1701 passed.
flywheel-core src untouched. No type diagnostics on changed `.py`.

## Honest limits

- #1's land/block is NOT exercised by a single end-to-end blind oracle that drives
  the full worker lease/submit. Its authoritative component (the gate PASS/FAIL
  verdict) is blind-graded by A; the activation that feeds the source is blind-
  graded by C; the worker-drive composition (no-merge-on-fail, park, record) is the
  00050 landing integration (already fw-verified, audit
  `00050-held-out-landing-gate-verify.md`) plus the green orchestrator/worktree
  suites. The recursion's NEW surface (activation + registration shape + template)
  is what 00051's blind oracles target directly.
- B's and C's blind tests pass against shipped code by design (they assert a
  contract that holds); their discrimination is established by the concrete mutant
  kills in `mutants_check.py`, not by a green test alone.
- A held-out suite is a filter, not a correctness proof: a finite oracle can still
  be slipped past. This audit proves a discriminating oracle EXISTS for the
  loop-closing behavior and that it grades the SHIPPED code — and, uniquely for
  00051, that an admitted oracle can now gate a real run through the activated gate.

## Reproduce

```
uv run pytest .flywheel/verification/00051/ -q          # 13 passed (blind oracles vs shipped)
uv run python .flywheel/verification/00051/mutants_check.py   # B KILL, C KILL
uv run pytest packages/flywheel-orchestrator/tests/ packages/flywheel-worktree/tests/ packages/flywheel-core/tests/ -q  # 1701 passed
```
