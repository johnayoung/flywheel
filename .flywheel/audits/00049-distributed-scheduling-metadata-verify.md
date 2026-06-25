# fw-verify discrimination proofs — 00049 distributed scheduling metadata

Date: 2026-06-25. Stage: fw-verify (held-out oracle authoring), run POST-execute
(spec 00049 shipped to `main` at commits `8ad8bc6` / `779fcd9` / `5770a1e` /
`e5b8136`; this audit adds the blind held-out grade).

Each oracle was authored BLIND by an independent subagent from a fenced contract
(the one criterion + observable contract + exact public signatures; NO
implementation body, NO agent tests, NO reference solution), then proven to
DISCRIMINATE: it PASSES a synthesized correct reference and KILLS >=1 synthesized
plausible-wrong reference. Oracles live git-ignored under
`.flywheel/verification/00049/<unit>/`; only this proof is durable. As the
post-execute timing allows, each admitted oracle was also REAL-GRADED — its
`sut` re-pointed (via a thin adapter) at the shipped symbols.

Legend: ADMITTED = discrimination-proven + flake-stable (2 identical runs).
REAL-GRADE = oracle run against the shipped implementation.

## Routing (17 criteria)

- AUTHOR held-out oracle: #2; #3; #4+#5; #6+#7; #8+#9+#10; #11+#12; #15 (7 units).
- SKIP (un-gameable / graded out-of-band): #1 (value-type attrs — `command`),
  #13 (both-backends parametrization — the suite runs SQLite + Postgres),
  #14 (suite DoD — `command`), #16 (core-schema no-change — `command`/git-diff),
  #17 (ordinary-claim no-regression — committed claim suite + the E/F control cases).
- Routed to manual: none.

## Units

- **Unit A** `directory-metadata` — #2 (DirectoryWorkSource reads top-level
  `priority`/`required_capabilities`/`conflict_keys`; absent -> defaults). ADMITTED.
  - killed (4/4): always_defaults; keyerror_on_absent; drops_the_sets; swapped_sets. correct ref PASSED.
  - REAL-GRADE: **PASS** via direct probe (valid task files with a real command
    grader, since the blind fixture used `graders: []` which a real task does not
    validate — NOT a shipped bug; the foundation hit the same fixture-fidelity
    point on its Unit G). `DirectoryWorkSource(...).list_work()` carries priority 7,
    `{python,docker}`, `{db-migration,schema}` on the declaring file and defaults on the omitting file.

- **Unit B** `persist-metadata` — #3 (non-default metadata persists into the
  `work_items` columns, not the 00048 forward-compat defaults). ADMITTED.
  - killed (4/4): constant_defaults; drops_sets; raw_set_repr (non-JSON); swapped_columns. correct ref PASSED.
  - REAL-GRADE: **PASS** against `SqliteClaimStore.upsert_work_item` /
    `load_work_item`: priority 7 and the two `*_json` columns decode (as sets) to
    the supplied sets; a default item reads 0 / `[]` / `[]`.

- **Unit C** `priority-ordering` — #4,#5 (descending priority, equal-priority
  ties keep walk order via a stable sort; all-default == walk order). ADMITTED.
  - killed (3/4): ignores_priority; ascending_priority; descending_tiebreak_by_id.
  - survived (equivalent mutant, tolerated): `descending_unstable_reversed_runs` —
    Python's `sorted(reverse=True)` IS stable, so this variant is behaviorally
    identical to correct; unkillable by any test. The id-tie-break kill is what
    proves the stability requirement discriminates.
  - REAL-GRADE: **PASS** against `WorkGraph.ready_set` offer order ([1,9,5]->9,5,1;
    equal priorities preserve walk order; all-default == input order).

- **Unit D** `capability-filter` — #6,#7 (item selectable iff
  required_capabilities subset of worker's set; empty-req selectable by any worker). ADMITTED.
  - killed (4/4): ignores_capabilities; empty_req_needs_something; overlap_instead_of_subset; inverted_subset. correct ref PASSED.
  - REAL-GRADE: **PASS** against `WorkGraph.ready_set(..., worker_capabilities=...)`:
    C={gpu} selects {none,gpu}; C={gpu,cuda} selects all three; C={} selects only {none}.

- **Unit E** `conflict-exclusion` — #8,#9,#10 (refuse a claim overlapping a
  different live claim's keys; disjoint/empty never refuse; clears on release).
  ADMITTED. **Headline reward-hack (no two conflicting items run at once).**
  - killed (4/4): ignores_conflict_keys (THE hack — concurrent conflicting claims);
    over_broad_exclusion (blocks any 2nd claim); never_clears_on_release;
    equal_keys_only (misses partial overlap). correct ref PASSED.
  - REAL-GRADE: **PASS** against `acquire_claim(..., conflict_keys=...)` /
    `release_claim`: B sharing A's key is refused while A is held; disjoint/empty
    C and D acquire concurrently; partial-overlap E refused; after releasing A, B acquires.

- **Unit F** `liveness-sweep` — #11,#12 (sweep batch-releases lapsed claims,
  leaves valid ones, reaps all of one worker's lapsed in one pass). ADMITTED.
  - killed (2/4 via the blind harness): reaps_live_claims; never_reaps.
  - survived under the injected-sut harness: one_at_a_time and off_by_one_boundary —
    the oracle's multi-lapse (case B) and the exact-boundary case instantiate
    `correct_reference()` INTERNALLY rather than the injected sut, so only case A
    (mixed lapsed/valid + ownership) drives a wrong store. Honest oracle-structure
    limitation, not a shipped defect.
  - REAL-GRADE: **PASS**. Case A graded against real `sweep_expired_claims` /
    `list_claims`; AND a direct probe closed the two survivors against the SHIPPED
    store: one sweep at t=10 releases all three of one worker's lapsed claims
    (#12 all-in-one-pass) and the expiry==now boundary is swept (the `<=` rule).

- **Unit G** `migration` — #15 (additive, non-destructive v2->v3; pre-existing
  rows survive; new feature works). ADMITTED.
  - killed (3/3): destructive_migration (seeded rows vanish); hard_version_mismatch
    (refuses a sub-3 store); sentinel_only_no_column (bumps sentinel, never adds the
    column, so conflict-key acquire fails). correct ref PASSED.
  - REAL-GRADE: **PASS**. A raw-sqlite3 v2 fixture (sentinel=2, `task_claims`
    WITHOUT `conflict_keys_json`, one seeded claim + one seeded work_item) opened
    with the real `SqliteClaimStore`: sentinel converged to 3, both seeded rows
    survived, and `acquire_claim(..., conflict_keys={"k"})` worked — confirming the
    shipped 2->3 migration is additive (`ALTER TABLE`), not drop-and-recreate.

## Summary

- Authored: 7 blind held-out oracles, all ADMITTED (discrimination-proven, flake-stable).
- Against-real-impl held-out grade: **7/7 PASSED** (A via a valid-task-file direct
  probe; F's two oracle-structure survivors closed by a direct multi-lapse + boundary
  probe against the shipped store).
- Honest limits: (1) Unit C carries one unkillable equivalent mutant
  (`reverse=True` stability) — tolerated, the id-tie-break kill proves the
  requirement. (2) Unit F's blind harness only drives a wrong sut through case A;
  #12 and the boundary were real-graded by a supplementary direct probe, not the
  blind kill-set. (3) This stage proves blind that a discriminating oracle EXISTS
  and that the shipped code passes it; the oracles are git-ignored and do NOT gate
  future agent runs. The durable regression guard stays the tasks' own command
  graders (`uv run pytest packages/flywheel-orchestrator/tests/`) + the committed
  suites. An execute-time held-out gate still needs out-of-worktree grading owned
  by the orchestrator (out of scope — the complementary work flagged on the
  foundation too).
