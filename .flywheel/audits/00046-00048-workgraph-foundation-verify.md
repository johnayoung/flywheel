# fw-verify discrimination proofs — WorkGraph foundation (specs 00046 / 00047 / 00048)

Date: 2026-06-25. Stage: fw-verify (held-out oracle authoring), run POST-execute
(the foundation three shipped to `main` at commit `5a9bd27`; this audit adds the
blind held-out grade the pre-execute run never produced).

Each oracle was authored BLIND by an independent subagent from a fenced contract
(no implementation, no agent tests, no reference solution), then proven to
DISCRIMINATE: it PASSES a synthesized correct reference and KILLS >=1 synthesized
plausible-wrong reference on a real input. Oracles live git-ignored under
`.flywheel/verification/<spec>/<unit>/`; only this proof is durable. As a bonus the
shipped timing allows, each admitted oracle was also run against the REAL shipped
code (`sut` re-pointed at the real symbols) — a genuine held-out grade.

Legend: ADMITTED = discrimination-proven + flake-stable (2 identical runs).
REAL-GRADE = oracle run against the shipped implementation.

## Spec 00046 — execution-mode guard

- **Unit A** `exec-mode-matrix` — criteria #1,#4,#5,#6 (`load_policy` mode matrix). ADMITTED.
  - killed: blanket-reject-all-distributed (fails #4); absent-table defaults to distributed (fails #1); unknown mode coerced to local (fails #6); distributed+sqlite raises an unkeyed message lacking path/distributed/store (fails #5 match). correct ref PASSED.
  - REAL-GRADE: **9 passed** against `flywheel_orchestrator._policy`. Shipped guard confirmed conditional (not a blanket ban), keyed, strict-validating.

## Spec 00047 — WorkGraph validation

- **Unit B** `edges` — #1,#2,#3 (build records chain/fan-out/fan-in edges). ADMITTED.
  - killed: no-op empty-edge builder; first-prereq-only (drops fan-in 2nd edge); parent-indexed last-child-wins (drops fan-out sibling); topo-order echoes input ignoring edges. REAL-GRADE: **9 passed**.
- **Unit C** `structural` — #4,#5,#6 (dup/self/cycle hard-fail naming ids). ADMITTED.
  - killed: silent dedup (no raise on dup); self-edge treated harmless; cycle detected but no member ids named; only-2-cycles detector misses 3-cycle. REAL-GRADE: **8 passed**.
- **Unit D** `missing-prereq` — #7,#13 (missing prereq = non-fatal issue, not a raise; stays out of ready_set; single + cross-source). ADMITTED.
  - killed: hard-fail on missing prereq (the D-1 regression); silent (no issue recorded); dangling task still in ready_set; per-source validation flags a resolvable cross-source edge. REAL-GRADE: **9 passed**.
- **Unit E** `ready-set` — #8,#9,#10,#11 (runnable query semantics). ADMITTED.
  - killed: prereqs ignored (children leak); DONE inverted (child never promoted); early-return serializes parallel children; `excluded`/own-state ignored (re-offers held task). REAL-GRADE: **10 passed**.
  - contract note: real `TaskState` non-eligible members are `IN_PROGRESS`/`AWAITING_APPROVAL`/`DONE` (the fence said RUNNING/CLAIMED); oracle used the real enum.
- **Unit F** `cross-source` — #12 (aggregated cross-source edge resolves, no spurious issue). ADMITTED.
  - killed: per-source isolation flags the edge missing; first-source-only drops the target; raises on the cross-source edge. REAL-GRADE: **5 passed**.

## Spec 00048 — orchestrator persistence v2

- **Unit G** `provenance` — #2,#3 (sources stamp provenance; metamorphic content->version). ADMITTED (re-authored).
  - killed: directory constant source_version (fails ==task_digest and the differ relation); github constant/url-hashed version (fails varies-with-body); wrong source_kind. correct ref PASSED.
  - REAL-GRADE: **8 passed** against `flywheel_orchestrator._sources.DirectoryWorkSource` + `_github.GithubWorkSource`.
  - history: the FIRST G oracle was admitted but its real-impl grade was inconclusive — the fence pack under-pinned two construction facts (real `DirectoryWorkSource` scans `<tasks_dir>/active/<phase>/*.json`, not flat; `GithubWorkSource(*, repo, label, default_graders, runner)` is keyword-only with required `label`). NOT a shipped bug. The fence was re-pinned with the exact construction shapes (signatures are fair fence content) and a fresh blind author re-authored the oracle, which now discriminates AND passes the real sources. A direct probe independently confirmed the same.
- **Unit H** `work-items` — #4,#5,#6 (catalog lifecycle). ADMITTED.
  - killed: reset first_seen_at on re-observe; stale disappeared_at after re-observe; hard-delete absent items; drop dependency edges. REAL-GRADE: **5 passed** against `sync_work_source` + `SqliteClaimStore`.
- **Unit I** `source-syncs` — #7,#8 (sync recording + failed-pass safety). ADMITTED. **Highest-stakes (reward-hack).**
  - killed: error path marks prior items disappeared (THE reward-hack); failure swallowed and recorded ok; observed_count hardcoded 0; finished_at unset on ok. REAL-GRADE: **4 passed** — shipped code confirmed: a failed `list_work()` marks nothing disappeared.
- **Unit J** `migration` — #9 (additive v1->v2; claims survive). ADMITTED.
  - killed: destructive drop-and-recreate (claim vanishes); hard version-mismatch raise on older sentinel; sentinel bumped but new table never created. correct ref PASSED.
  - REAL-GRADE: **PASSED** — a genuine v1 fixture was built with raw sqlite3 matching the real `task_claims` schema (sentinel=1, new tables ABSENT, one seeded claim), then opened with the real `SqliteClaimStore`: the sentinel converged to 2, the seeded `claimed-1` row survived, and `upsert_work_item` landed in the now-present `work_items` table. The shipped migration is additive/non-destructive (no drop-and-recreate, no version-mismatch raise).

## Summary

- Authored: 10 blind held-out oracles, all ADMITTED (discrimination-proven, flake-stable).
- Against-real-impl held-out grade: **10/10 PASSED**. A,B,C,D,E,F,H,I via the oracle run directly against real symbols; G via a re-authored oracle after the fence was re-pinned with exact construction shapes; J via the admitted oracle plus a faithful real-store v1-fixture grade.
- Routed SKIP (un-gameable / DoD, graded out-of-band): 00046 #2,#3,#7,#8; 00047 #14,#15; 00048 #1,#10,#11.
- Routed to manual: none.
- Honest limit: this stage proves blind that a discriminating oracle EXISTS and (here) that the shipped code passes it. The oracles are git-ignored and do NOT gate future agent runs; the durable regression guard stays the tasks' own command graders + the committed suites. An execute-time held-out gate needs out-of-worktree grading owned by the orchestrator (out of scope).
