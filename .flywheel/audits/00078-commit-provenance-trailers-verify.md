# fw-verify record: 00078-FEATURE-commit-provenance-trailers

**Verified:** 2026-07-09
**Spec:** `.flywheel/specs/00078-FEATURE-commit-provenance-trailers.md`
**Tasks:** `.flywheel/tasks/active/19-commit-provenance-trailers/` (3 tasks)
**Oracles (git-ignored scratch):** `.flywheel/verification/00078-commit-provenance-trailers/{W1_merge_landing_trailers,W2_pr_push_trailers}/test_*.py`; execute-time adapters `<unit>/driver.py` + `<unit>/sut_real.py`
**Registrations:** `merge-landing-trailer-stamp.json` (W1), `pr-push-trailer-stamp.json` (W2)

## Routing

- AUTHOR: criteria 1+3+4 (unit W1, one landing scenario asserting all three facets), criterion 2 (unit W2).
- SKIP (visible, owned by the task's own graders): criteria 5, 6 (`fw show` commit lookup — task `fw-show-commit-lookup` carries no registration).
- SKIP (already un-gameable at operator scope): criterion 7 (verification-surface DoD — enforced by `[phase] verify` / `[submit] verify` plus diff review).

## Discrimination proofs (re-run independently by the orchestrating session)

All 8 wrong references re-killed and both correct references re-passed by this session (not agent claims); flake screen run-twice stable on both units.

| Unit | Criteria | Kills |
| --- | --- | --- |
| W1 merge-landing-trailers | #1, #3, #4 | no-stamping plain FF; forged-value keeper (adds missing keys only); range squashed to one commit; tip-tree stamping (trailers correct, trees wrong — only the ordered tree relation catches it) — 4/4 |
| W2 pr-push-trailers | #2 | push-unstamped-then-log-create; stamp-locally-push-prestamp-sha; forged-value keeper; log-create-without-push — 4/4 |

Oracle forms: metamorphic (trailer-per-commit relation over the landed/pushed range via `git interpret-trailers` / `%(trailers:...)`, plus ordered tree-identity between pre-land and landed ranges).

## Null-reference kill on the real system

Both drivers RED (exit 1) against current main through the REAL landing paths, with the exit-3 DRIVER-ERROR channel proven distinct (marker-based mapping; a simulated construction failure exits 3, the genuine nulls exit 1):

- W1: real `GitWorktreeSubmitter.submit` clean-FF (out-of-tree `git fetch . branch:base` rung) — `trailer Flywheel-Task must appear exactly once ... got []`; forged `forged-123` survived. The trees-identical facet passes pre-implementation, as expected.
- W2: real `GitPullRequestSubmitter.submit` (push + gh create/edit via injected runner) — `remote commit ... is missing trailer Flywheel-Task`; the create-then-edit refresh flow itself worked before the trailer assertion tripped.

## Bridging assumptions (adapter, not oracle, risks)

1. W2's adapter mirrors the real pushed branch name (`flywheel/<phase>/<task_id>`) to alias the oracle-visible remote ref; a branch-naming change in the implementation mislabels W2 RED until the adapter is updated.
2. Trailer values enter via `SubmitRequest.run_id`/`task_id` and the task file's phase directory; an implementation sourcing them from store lifecycles instead will read RED (flagging for adjudication, arguably correct discrimination).
3. Commits are transplanted by cherry-pick (no commit hooks run), so a hooks-only stamping implementation stays RED — consistent with spec D-1 (land-time stamping is the mechanism).
4. Adapter defaults: `verify_command=None`, `held_out_source=None`, retention `on_done="destroy"`, `protected_paths=()`.

## Honest limits

- This stage proves blind that discriminating oracles exist and records the proof; the execute-time gate on the real runs is the registrations above (read out-of-worktree by the orchestrator) plus each task's own command graders.
- A DRIVER-ERROR (exit 3) at gate time is fail-closed (blocks landing) and requires operator adjudication — it is an adapter defect signal, not a verdict.
- W1's null run exercised the out-of-tree FF rung; an implementation stamping only the in-tree `merge --ff-only` rung would still read RED at gate time if the gate scenario takes the other rung — coverage, not an artifact.

## Fences

All three task briefs in `19-commit-provenance-trailers/` carry "Do not read or write under .flywheel/verification/" in `non_goals` (applied at plan time).
