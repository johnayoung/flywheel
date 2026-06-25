# fw-verify discrimination proofs — 00053 PR review-thread work source

Date: 2026-06-25. Stage: fw-verify (held-out oracle authoring), run POST-execute
(spec 00053 shipped to `main` at commits `cac3d42` adapter / `975246d` policy +
registry wiring). 00053 adds a third GitHub adapter: an UNRESOLVED PR review
thread becomes a task graded by the operator's out-of-band default graders (plus
the 00050/00051 held-out gate) — never by the thread's `isResolved` state, which
is a candidate filter, never the verdict (the anti-hack crux, mirroring 00052's
stance on the GitHub check status). This audit blind-grades that adapter.

Each oracle was authored BLIND by an independent subagent from a fenced contract
(the criterion + the stable `WorkSource`/`WorkItem` seam + the injectable `gh`
runner + GitHub's public GraphQL `reviewThreads` shape; the new
`_github_review.py` adapter body was fenced out — authors confirmed the
`GithubReviewWorkSource` `__init__` signature via `inspect` only, never reading
the source or the agent's own `test_github_review*`). Oracles live git-ignored
under `.flywheel/verification/00053/<unit>/`; only this proof is durable. Every
oracle was REAL-GRADED against the shipped adapter, and each additionally KILLS a
concrete MUTANT (`.flywheel/verification/00053/mutants_check.py`).

Discovered surface (blind, via `inspect.signature`):
`GithubReviewWorkSource(*, repo, default_graders=(), runner=None, log=None)`,
`source_kind = "github_review"`.

Legend: ADMITTED = discrimination-proven + flake-stable. REAL-GRADE = run against
the shipped implementation. KILL = oracle red against a deliberately-wrong
variant of the shipped adapter, green against shipped.

## Routing (10 criteria)

- AUTHOR held-out oracle:
  - A — #1 + #5 (an unresolved thread becomes one WorkItem graded by EXACTLY the
    operator's default graders; resolved threads excluded; no grader derived from
    `isResolved`).
  - B — #3 (stable per-thread-node-id keying; the unbounded-duplicates hazard on
    every reply).
  - C — #6 (malformed / failed listing raises `WorkSourceError`, never `[]` read
    as "all threads resolved" — the fail-open anti-regression).
  - D — #7 (report() posts a PR comment and issues NO thread-resolution
    mutation — the one untrusted signal).
- SKIP / real-grade probe (un-gameable, already command-graded by fw-plan):
  - #2 (grader-less skip+log) — probe: `default_graders=()` -> `list_work()==[]`
    and a `[github_review] skipping ...` log line. GREEN.
  - #4 (source_version changes on a new reply, id stable) — probe: same node id +
    appended comment -> identical `task.id`, different `source_version`. GREEN.
  - #8 (policy kind+repo builds the source) — probe: `load_policy` on
    `kind="github_review"` + repo -> `source_kind=="github_review"`,
    `build_work_source` -> `GithubReviewWorkSource` bound to the repo. GREEN.
  - #9 (missing repo -> PolicyError) — probe: `kind="github_review"` without
    `repo` -> `PolicyError` naming `repo`. GREEN.
  - #10 (full thread + URL in context) — probe: `context.notes` carries every
    author+body, `context.references` carries the thread URL. GREEN.
- Routed to manual: none.

## Admitted oracles (blind, discrimination-proven, real-graded)

### A — unresolved thread carries the operator's graders; resolved excluded (#1, #5)
`.flywheel/verification/00053/A-grader-source/test_review_grader_source.py`
- A 2-thread payload (one `isResolved:false`, one `isResolved:true`) +
  `default_graders=(CommandGrader(run="uv run pytest"),)` -> exactly ONE WorkItem,
  for the unresolved thread (`source_ref`/`source_url`/`id` name the unresolved
  thread, never the resolved one), `task.graders == default_graders` exactly, id
  prefixed `prc-`, and no grader names/derives from `resolv*`.
- REAL-GRADE: GREEN against shipped `_compile_thread` (resolution is the filter at
  line 258, graders are `list(self.default_graders)` at 290).
- KILL: an `IncludesResolved` mutant (neutralizes the `isResolved` skip) emits 2
  WorkItems -> oracle RED (`expected exactly one WorkItem, got 2`).

### B — stable per-thread-node-id keying (#3)
`.flywheel/verification/00053/B-stable-keying/test_review_stable_keying.py`
- Same thread node id `PRRT_alpha` listed twice (second has an appended reply) ->
  identical `task.id`; a different node id `PRRT_beta` -> different id.
- REAL-GRADE: GREEN against shipped `_review_item_id` (digest of the node id only).
- KILL: a per-call-counter keying mutant yields `prc-0000…1` then `prc-0000…2` for
  the same thread -> oracle RED (`id must be stable per thread node id`). This is
  the unbounded-duplicates-on-every-reply hazard D-3 forecloses.

### C — fail-closed listing (#6)
`.flywheel/verification/00053/C-fail-closed/test_review_fail_closed.py`
- Invalid JSON, a top-level list, an empty object, a missing `data.repository`, a
  thread missing its node id, and a raising runner each raise `WorkSourceError`; a
  well-formed anchor payload still yields work (guards against a degenerate
  always-raise passing for the wrong reason).
- REAL-GRADE: GREEN against shipped `_require_object`/`_nodes` + the node-id guard
  (lines 250-254).
- KILL: a `FailOpen` mutant (swallows `WorkSourceError` -> `[]`) makes the
  malformed input return `[]` -> oracle RED (`list_work fell open`). This is the
  "parse break masquerades as a green repo" anti-regression.

### D — report() never resolves a thread (#7)
`.flywheel/verification/00053/D-report-no-resolve/test_review_report_no_resolve.py`
- After `report()` with `source_ref="o/r#42#PRRT_xyz"`: a `gh pr comment 42 --repo
  o/r --body <non-empty>` call IS recorded, and NO recorded call contains
  `resolveReviewThread` (nor any `graphql`+`resolve` mutation).
- REAL-GRADE: GREEN against shipped `report` (a single `gh pr comment`, no
  mutation; structurally there is no resolve call anywhere in `src/`).
- KILL: an `AlsoResolves` mutant (posts the comment AND issues
  `resolveReviewThread`) -> oracle RED on the resolve call. This is the headline
  D-5 anti-hack: the harness must never flip the one untrusted signal.

## Discrimination gate result

`uv run python .flywheel/verification/00053/mutants_check.py` -> **Killed 4/4
mutants.** `uv run pytest .flywheel/verification/00053/` -> 17 passed, stable
across two runs (flake screen). Every oracle both passes the shipped code and
kills a concrete plausible-wrong variant; none is a green-test-that-grades-nothing.

## Honest limits

- **D-4 subjective-intent gap (recorded, not a defect).** The grade is the
  operator's default graders run out-of-band; those may not capture the reviewer's
  specific subjective concern (the same shape as 00052 D-2). The out-of-band-ness
  and the grade's independence from `isResolved` are PROVEN (oracle A pins
  grader-tuple identity; the source reads `isResolved` only as the filter at line
  258; report() issues no resolve). Capturing subjective intent is the operator's
  to align — tune `[defaults.graders]` and/or register a held-out grader keyed by
  the `prc-<digest>` task id (composes with 00051). It is never auto-derived (the
  rejected rubric path).
- **#5 end-to-end composition.** The "driven to DONE only on out-of-band graders"
  property is verified by composition: oracle A proves the compiled Task carries
  exactly the operator's graders (no resolution-derived grader); the orchestrator's
  drive/grade path is 00050's already-verified landing integration; the green
  orchestrator suite (1685 passed on integrated main) exercises the drive. It is
  not a single end-to-end blind oracle.
- **Registration deferred.** No `[held_out] root` is configured in `flywheel.toml`
  (only `sandbox_root`), so there is nowhere to write a
  `<held-out-root>/<task_id>.json` registration; the durable proof is this audit +
  the tasks' own committed command graders (orchestrator + core suites) + CI. When
  a held-out root is set, oracles A-D can be registered to gate the real run
  out-of-band (the 00051 channel).

## Fence (operator applies if re-running these tasks)

`non_goals`: "Do not read or write under `.flywheel/verification/`" — keeps a
future blind oracle for the same criterion out of the implementing agent's view.
(The 00053 tasks are already archived; recorded here for the record.)

This stage proves BLIND that a discriminating oracle exists for each held-out
behavior and records that proof. The execute-time gate on the agent's real work
stays the task's own command graders and tests (the durable, CI-run guard). A
held-out suite is a filter, not a correctness guarantee.
