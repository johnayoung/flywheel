# fw-verify discrimination proofs — 00052 GitHub CI-failure work source

Date: 2026-06-25. Stage: fw-verify (held-out oracle authoring), run POST-execute
(spec 00052 shipped to `main` at commits `bc035f2` adapter / `f2f9e3d` loop
integration). 00052 adds a second GitHub adapter: a failing CI run becomes a task
graded by the operator's out-of-band default graders (never the GitHub check
status). This audit blind-grades that adapter.

Each oracle was authored BLIND by an independent subagent from a fenced contract
(the criterion + the stable `WorkSource`/`WorkItem` seam + the injectable `gh`
runner; the new `_github_ci.py` adapter body was fenced out — authors discovered
the `GithubCiWorkSource` class name + `__init__` signature via `inspect`, never
reading the source). Oracles live git-ignored under
`.flywheel/verification/00052/<unit>/`; only this proof is durable. Every oracle
was REAL-GRADED against the shipped adapter, and B/C additionally against a
concrete MUTANT (`.flywheel/verification/00052/mutants_check.py`).

Discovered surface (blind, via `inspect.signature`):
`GithubCiWorkSource(*, repo, default_graders=(), failure_filter="failure",
runner=None, log=None)`.

Legend: ADMITTED = discrimination-proven + flake-stable. REAL-GRADE = run against
the shipped implementation. KILL = oracle red against a deliberately-wrong
implementation, green against shipped.

## Routing (12 criteria)

- AUTHOR held-out oracle:
  - A — #2 (a failed run carries the operator's default graders; a grader-less CI
    item is skipped, never run ungated).
  - B — #5 (stable per-(workflow, branch) id across polls; the unbounded-dup
    hazard).
  - C — #6 (a now-fixed failure disappears from a successful listing; a malformed
    listing raises `WorkSourceError`, never `[]` — the 00048 fail-open anti-regression).
- SKIP / real-grade probe:
  - #1 (lists failed runs) — argv probe: `gh run list --status failure --json ...`.
  - #3, #4 (out-of-band grade / disable-the-check anti-hack) — composed: oracle A
    proves the item's graders ARE the operator's default graders (no CI-status
    grader); the only `gh` calls the adapter issues are `run list` (listing) and
    `api .../commits/<sha>/comments` (write-back), neither reads check status as a
    verdict; the operator's default graders (e.g. pytest) are independent of
    `.github/workflows/`, so disabling the CI step cannot make them pass; and
    00051's fw-verify already proved out-of-band graders discriminate through the
    gate. No GitHub/agent status is ever the grade.
  - #7 (provenance) — probe: `source_kind="github_ci"`, `source_url` carries the
    run URL, `source_version` changes when the head sha changes.
  - #8 (report write-back) — probe: `report()` on DONE issues
    `gh api --method POST repos/<repo>/commits/<sha>/comments -f body=...`; no
    issue is mutated.
  - #9 (malformed -> error) — covered by oracle C.
- SKIP (un-gameable / out-of-band): #10 (issue+directory suites unchanged), #11
  (no-core-change — `git diff bc035f2^..HEAD` touches no `flywheel-core/src`), #12
  (suites — `command`).
- Routed to manual: none.

## Admitted oracles (blind, discrimination-proven, real-graded)

### A — CI item carries the operator's graders; grader-less is skipped (#2)
`.flywheel/verification/00052/A-grader-source/test_ci_grader_source.py`
- One failed run + `default_graders=(CommandGrader(run="pytest"),)` ->
  `item.task.graders == default_graders`; same run + empty defaults ->
  `list_work() == []` (skipped, never returned ungated).
- Kills: (a) an adapter returning an empty-grader item (runs ungated) — fails the
  skip assertion; (b)/(c) an adapter that fabricates or drops graders — fails the
  equality assertion.
- REAL-GRADE: `2 passed` against shipped adapter. ADMITTED.

### B — stable per-(workflow, branch) keying (#5)
`.flywheel/verification/00052/B-stable-keying/test_ci_stable_keying.py`
- Two polls of the SAME (workflow="ci", branch="main") with DIFFERENT
  databaseId+headSha -> SAME id (`ci-052a87db93c41935`); different workflow/branch
  -> different id.
- REAL-GRADE: `2 passed`.
- KILL (concrete, `mutants_check.py`): a databaseId-keyed mutant ->
  `shipped_stable=True mutant_stable=False -> KILL`. The equality assertion catches
  the unbounded-re-queue thrash (a new id every poll/push).
- ADMITTED.

### C — disappearance + fail-closed listing (#6, #9)
`.flywheel/verification/00052/C-disappearance/test_ci_disappearance.py`
- A present failure -> one item; a SUCCESSFUL empty listing -> `[]` (disappears);
  a MALFORMED / non-JSON listing -> `WorkSourceError` (not `[]`). (The author probed
  both surfaces: malformed text is wrapped as `WorkSourceError`, the documented
  surface; a runner that raises propagates raw — recorded honestly.)
- REAL-GRADE: `2 passed`.
- KILL (concrete, `mutants_check.py`): a fail-OPEN mutant that swallows the error
  and returns `[]` -> `shipped_raises=True mutant_raises=False mutant_returns_empty=True
  -> KILL`. The `pytest.raises` assertion catches the dangerous "tracker hiccup read
  as CI-is-green" fail-open (the 00048 anti-regression).
- ADMITTED.

## Real-grade probes (shipped code)

- #1: `list_work()` issues `gh run list --status failure --json ...` for the repo.
  Confirmed.
- #7: a compiled item carries `source_kind="github_ci"`, a `source_url` with the
  run URL, and a `source_version` that CHANGES when the head sha changes. Confirmed
  (`source_version` is a digest of sha+conclusion, not the literal sha — the
  change-detection property holds).
- #8: `report()` on DONE posts a commit comment
  (`gh api --method POST repos/<repo>/commits/<sha>/comments`); issues no
  `issue close`/comment. Confirmed.
- #3/#4: the item's graders are exactly the operator's default graders (oracle A);
  the adapter issues no status-reading `gh` call in the grade path (only `run list`
  + commit-comment); out-of-band graders are independent of the workflow file and
  were already proven discriminating in 00051. Composed-confirmed.
- #11 no-core-change: `git diff` touches no `packages/flywheel-core/src/`. Confirmed.

## Aggregate

3 blind oracles (A/B/C), all ADMITTED. Real-graded against shipped code: 6/6 oracle
assertions pass; B and C each KILL a concrete mutant (databaseId-keying, fail-open
listing); A discriminates by construction (graders-carried vs skipped). 5 probe
criteria (#1/#7/#8/#3-4/#11) real-graded green. Full suite green on integrated main:
1732 passed. flywheel-core src untouched. No type diagnostics on changed `.py`.

## Honest limits

- D-2's accepted limitation stands: the grade is the operator's `[defaults.graders]`,
  which may NOT be the same check that failed in CI. The audit proves the grade is
  out-of-band and never the GitHub status; aligning the default graders with what
  the operator cares about is the operator's responsibility (the spec grades
  out-of-band-ness, not CI-check reproduction).
- #4 (disable-the-check anti-hack) is verified by composition (oracle A + the
  pytest/workflow-file independence + 00051's gate discrimination), not by a single
  end-to-end orchestrate-drive blind oracle that commits a workflow edit. The
  shipped `test_github_ci_loop_integration.py` exercises the drive directly.
- A held-out suite is a filter, not a correctness proof.

## Reproduce

```
uv run pytest .flywheel/verification/00052/ -q          # 6 passed (blind oracles vs shipped)
uv run python .flywheel/verification/00052/mutants_check.py   # B KILL, C KILL
uv run pytest packages/flywheel-orchestrator/tests/ packages/flywheel-worktree/tests/ packages/flywheel-core/tests/ -q  # 1732 passed
```
