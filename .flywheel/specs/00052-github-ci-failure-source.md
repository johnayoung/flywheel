# 00052 — GitHub CI-failure work source

Status: spec. Extends the WorkSource direction (flywheel as a project-agnostic
verified execution kernel) with a SECOND GitHub adapter: a failing CI run becomes
a work item the loop drives to green. The shipped `GithubWorkSource` lists labeled
ISSUES only (`_github.py`); this adds a `kind = "github_ci"` source that lists a
repo's FAILED continuous-integration runs and compiles each into a `Task` graded
out-of-band by the operator's default graders (and, when configured, 00051's
held-out gate). flywheel-core is untouched; this is a new orchestration-layer
adapter behind the existing `WorkSource` seam.

## Outcome

An operator points `flywheel.toml` at `[source] kind = "github_ci"` for a repo.
The worker lists that repo's failing CI runs and, for each one whose work is
gradeable by the operator's declared default graders, drives a task to fix it. The
authoritative "fixed" signal is the harness running those default graders
out-of-band (never the GitHub check status and never the agent's word); a fix that
lands does so through the normal submit strategy and inherits 00051's held-out
gate when one is configured. A persistently-failing CI job is ONE stable work item
across polls, not a new item every poll or push. When the job next passes (or the
failure is gone), the item disappears and any in-flight run for it is interrupted;
a transient `gh` listing failure marks NOTHING disappeared. On a terminal outcome
the source writes the grader receipts back to GitHub (a comment on the run's commit
or PR) and never mutates an unrelated issue. The existing `github` (issues) and
`directory` sources are unchanged.

## Background

flywheel already turns labeled GitHub issues into graded tasks (shipped
2026-06-09; `_github.py`, `WorkSource` seam in `_sources.py`). The natural next
inbound signal is CI: a red build is unambiguous, operator-owned work, and it
composes with the verified-conformance loop just closed in 00051 — a CI failure
becomes a task whose landing is gated by the operator's real checks. The tacit,
load-bearing requirement a literal agent will miss: the value is in the grade
being the operator's OWN out-of-band check, not "CI is green." The cheapest way to
fake a CI fix is to make the build pass without fixing the code — delete or skip
the failing step, edit the workflow file, or trust an agent-triggered re-run. So
the grade can never be the GitHub check status or a re-run the agent can influence;
it must be the operator's default graders (e.g. the test suite) executed by the
harness out-of-band, exactly as every other flywheel task is graded. A second
load-bearing subtlety: a CI failure that re-appears every poll must be ONE item,
or the scheduler thrashes on an unbounded stream of duplicates.

This adapter slots behind the existing seam unchanged: `WorkSource.list_work() ->
Sequence[WorkItem]` and `report(WorkReport)` (`_sources.py:129-152`); the
provenance + scheduling fields on `WorkItem` (`_sources.py:52-92`); the source
registry dispatch (`_source_registry.py:20-35`); and the reconcile / `source_syncs`
storage posture where a failed listing never marks work disappeared
(`_orchestrate.py:172-280`, 00048 criterion #7). The `gh` CLI is reached through
the same injectable runner seam the issue source uses (`GhRunner`,
`_github.py:62`), so tests fake it.

## Scope

### In scope
- A new `WorkSource` adapter selected by `[source] kind = "github_ci"` (with a
  repo and an optional failure filter) that lists the repo's FAILED CI runs via
  the injected `gh` runner and compiles each into a `WorkItem`.
- Compiling a failed run into a `Task`: a goal naming the failing CI work, a brief
  locating the failure (workflow/job identity, head sha, run URL), and graders
  taken from the operator's `[defaults.graders]` policy.
- The readiness gate: a CI item that resolves to ZERO graders is skipped and
  logged, never returned as runnable (mirrors the issue source).
- Stable keying / dedup: a persistent failure on the same (workflow, branch) is
  one stable item id across polls; `source_version` tracks the failing head sha /
  conclusion so a changed failure is detected.
- Provenance stamps: `source_kind` for the CI source, `source_url` locating the
  run, `source_version` from the head sha + conclusion.
- `report()` write-back: on a terminal outcome, post a grader-receipt comment to
  the run's commit (or its PR) via the injected runner.
- Defensive listing: malformed `gh` output raises `WorkSourceError` rather than
  silently returning no work.
- Registry + `[source]` policy parsing for the new kind.

### Out of scope
- PR review threads as work (a separate, subjective-grader spec).
- Running the issue source AND the CI source simultaneously (multi-source policy);
  this spec ships the CI source standalone behind one `[source] kind`.
- Deriving `priority` / `required_capabilities` / `conflict_keys` from CI metadata
  (00049 D-7 stays deferred; CI items schedule at defaults).
- Re-running the exact failed CI check locally as the grade, or reading the GitHub
  check status / triggering a re-run as the grade — the grade is the operator's
  out-of-band default graders.
- Hardening the existing issue source (auth, rate-limit, pagination past its cap);
  orthogonal.
- Any change to flywheel-core: `Task`, `Grader`, the lifecycle, the harness, or
  core's schema.

### Must not regress
- The existing `github` (issues) and `directory` sources behave identically.
- The reconcile posture — a failed `list_work()` marks nothing disappeared
  (00048 criterion #7) — holds for the CI source.
- The `source_syncs` / `work_items` storage and the `WorkSource`/`WorkItem`
  contracts are used unchanged (no new store).
- flywheel-core's lifecycle, `Task`/`Grader`, and schema are unchanged.

## Success Criteria

Each criterion grades an observable end-state — the listed items, the compiled
task, the recorded write-back, the disappearance — never a call sequence. Tests
live under `packages/flywheel-orchestrator/tests/` with a fake `gh` runner,
mirroring `test_github_source.py`.

1. When the CI source lists work, it queries the repo's FAILED CI runs (not
   issues, not passing runs) through the injected `gh` runner and returns one
   `WorkItem` per failing run. [command | held-out]
   verify: a test with a fake runner asserts `list_work` issues a failed-CI-runs
   query for the configured repo and returns a `WorkItem` per failed run in the
   faked payload.
   defends against: listing issues / PRs / passing runs, or returning nothing
   while CI is actually red.

2. A CI-failure `WorkItem` carries the operator's `[defaults.graders]` as its
   graders; a CI item that resolves to zero graders is skipped and logged, never
   returned as runnable. [command | held-out]
   verify: a test asserts a failed run with configured default graders →
   `item.task.graders == default_graders`; with no default graders → the item is
   omitted from `list_work` and a skip is logged.
   defends against: running a CI fix ungated, or fabricating a grader the operator
   never declared.

3. A CI-failure item reaches DONE only when the operator's graders pass when run
   out-of-band by the harness; the source never reads or trusts the GitHub check
   status (or an agent-triggered re-run) as the grade. [command | held-out]
   verify: a test drives a CI item and asserts it reaches DONE only on out-of-band
   grader pass; a test asserts the source's grade path issues no `gh` call that
   reads check/run status as the verdict.
   defends against: trusting CI, GitHub, or the agent's self-report to declare the
   build fixed.

4. If the committed "fix" makes CI green by weakening the check itself (editing or
   disabling the failing workflow/CI step) without fixing the code, the
   out-of-band graders still fail and the item does not reach DONE. [command | held-out]
   verify: a test where the committed change disables the CI step but the
   operator's default graders (run out-of-band) still fail asserts the item is NOT
   DONE.
   defends against: the cheapest CI-fix hack — make the build pass by removing the
   check rather than fixing the behavior.

5. Two `list_work` passes over the SAME persistent failure (same workflow +
   branch) yield the SAME work item id, so the scheduler sees one item, not a new
   one per poll. [command | held-out]
   verify: a test lists twice over an unchanged failing run and asserts identical
   item id across passes.
   defends against: unbounded re-queueing — a new id every poll/push thrashing the
   scheduler.

6. When a previously-listed CI failure is no longer present (the job now passes or
   the run is gone), a SUCCESSFUL list pass omits it (so storage marks it
   disappeared and an in-flight run is interrupted); a FAILED list pass raises
   `WorkSourceError` and marks NOTHING disappeared. [command | held-out]
   verify: a test asserts (a) a now-passing job drops the item from `list_work`;
   (b) a `gh` listing error raises `WorkSourceError`, and through `sync_work_source`
   nothing is marked disappeared.
   defends against: a fixed failure lingering as zombie work, and a tracker hiccup
   reading as "all CI work vanished" (the 00048 anti-regression).

7. A compiled CI `WorkItem` stamps provenance: a `source_kind` identifying the CI
   source, a `source_url` locating the run, and a `source_version` that changes
   when the failing head sha / conclusion changes. [command | visible]
   verify: a test asserts `source_kind` / `source_url` / `source_version` on a
   compiled CI item, and that changing the faked head sha changes `source_version`.
   defends against: untraceable CI items, or failing to notice the failure moved
   to a new commit.

8. On a terminal outcome, `report()` writes the grader receipts back to GitHub via
   the injected runner (a comment on the run's commit or PR) and never closes or
   mutates an unrelated issue. [command | held-out]
   verify: a test asserts `report()` on DONE posts a receipt comment via `gh` to
   the run's commit/PR, and issues no `issue close`.
   defends against: silent landings with no audit trail, or mutating the wrong
   GitHub object.

9. Malformed `gh` output causes `list_work` to raise `WorkSourceError`, not return
   an empty list. [command | held-out]
   verify: a test feeds non-JSON / unexpected payload and asserts `WorkSourceError`.
   defends against: a parse break masquerading as "no CI failures" — treating the
   repo as green when listing actually failed.

10. The existing `github` (issues) and `directory` sources behave identically
    after this change. [command | held-out] (must-not-regress)
    verify: the existing `test_github_source.py` and directory-source suites pass
    unchanged.
    defends against: refactoring the github adapter and regressing the issue path.

11. flywheel-core is unchanged by this feature. [command | held-out] (verification-surface)
    verify: `uv run pytest packages/flywheel-core/tests/` passes unchanged and
    `git diff` touches no file under `packages/flywheel-core/src/flywheel_core/`.
    defends against: pushing CI-source concerns into core.

12. The orchestrator + worktree + core suites still pass. [command | held-out] (verification-surface)
    verify: `uv run pytest packages/flywheel-orchestrator/tests/`,
    `uv run pytest packages/flywheel-worktree/tests/`, and
    `uv run pytest packages/flywheel-core/tests/` all pass.
    defends against: satisfying a new criterion by weakening an existing source or
    scheduling test.

Verification surface: this feature adds an inbound adapter; the authoritative grade
stays the harness's out-of-band graders (unchanged). Definition of Done (inherited,
held-out where possible): the existing source suites pass (#10, #12); no
flywheel-core change (#11); the grade is the operator's out-of-band graders, never
GitHub status (#3, #4); a failed listing marks nothing disappeared (#6). No grading
assertion may be relaxed; a removed assertion with no equal-or-stronger replacement
is a blocking defect.

## Decomposition Hint (for /fw-plan)
- Layer **CI source: list + compile + readiness + provenance** (satisfies #1, #2,
  #5, #7, #9): the new adapter lists failed runs through the `GhRunner` seam,
  compiles each to a `WorkItem` with default graders, a locating brief, stable
  keying, and provenance stamps, skipping grader-less items and raising on
  malformed output. Plus the registry + `[source] kind = "github_ci"` parsing. No
  landing dependency.
- Layer **reconcile + disappearance** (satisfies #6): the now-passing/failed-listing
  behavior through the existing `sync_work_source` path. Depends on the source.
- Layer **report write-back** (satisfies #8): the terminal-outcome receipt comment.
  Depends on the source.
- Layer **out-of-band grade composition** (satisfies #3, #4): asserts the CI item's
  grade is the harness's out-of-band default graders (and the 00051 held-out gate
  when set), independent of GitHub status — largely composing shipped machinery via
  integration tests. Depends on the source supplying graders.

Shared invariants multiple layers assert against:
- The `WorkSource` / `WorkItem` contract (`list_work`/`report`, the provenance +
  graders fields) — used unchanged; every layer asserts against it.
- The stable item-id keying (workflow + branch) — defined by the source layer,
  relied on by reconcile (#6) and dedup (#5).

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-1: CI failures are a new source kind `github_ci`, separate from the issue source  (Status: Accepted)
- Context: the failing-CI signal has different listing, keying, and write-back than
  labeled issues.
- Decision: a distinct `WorkSource` adapter behind `[source] kind = "github_ci"`,
  composing through the existing seam and registry.
- Rejected: extending `GithubWorkSource` to also list runs (muddies the issue
  adapter, couples two listing/keying/write-back models); a flag on the issue
  source (same problem).
- Consequences: #1/#10 grade the new source without touching the issue path.

### D-2: The authoritative grade is the operator's default graders run out-of-band, never GitHub check status  (Status: Accepted)
- Context: the cheapest CI-fix fake is making the build pass without fixing the
  code; trusting GitHub/agent re-runs re-opens that.
- Decision: a CI item is graded by `[defaults.graders]` executed by the harness
  out-of-band (and 00051's held-out gate when configured), exactly like every
  flywheel task. The source never reads check status as the verdict.
- Rejected: GitHub check status / agent-triggered re-run as the grade (gameable,
  not out-of-band, violates "agent claims are untrusted"); re-running the exact
  named CI check locally (a check-run exposes a name, not a portably reproducible
  command — brittle across CI setups; may be offered later as an optional layered
  grader when the command is declared).
- Consequences: #3/#4 are load-bearing. The grader may differ from the exact check
  that failed in CI — the operator is responsible for aligning `[defaults.graders]`
  with what they care about; this is an accepted limitation, not an un-gradeable
  criterion (the spec grades that the grade is out-of-band, not that it reproduces
  the CI check).

### D-3: CI items are keyed stably per (workflow, branch); source_version tracks the failing head  (Status: Accepted)
- Context: a persistently-broken branch re-lists a failing run every poll/push; a
  new id each time would thrash the scheduler.
- Decision: the item id is stable for a given (workflow, branch); `source_version`
  carries the failing head sha / conclusion so a moved failure is detected.
- Rejected: per-run-databaseId keying (unbounded duplicates as the head advances).
- Consequences: #5 grades the stable id; #7 grades the changing `source_version`;
  #6 relies on the keying for clean disappearance.

### D-4: A CI item with no resolvable graders is skipped and logged, never run ungated  (Status: Accepted)
- Context: a CI run carries no `flywheel` spec block; its only graders are the
  operator's defaults.
- Decision: reuse the issue source's readiness gate — zero graders → skip + log,
  never a runnable item.
- Rejected: synthesizing a grader (fabricates a check the operator never declared);
  running ungated (lands unverified CI fixes).
- Consequences: #2 grades the skip-when-grader-less behavior.

### D-5: report() writes a receipt comment to the run's commit/PR; it never closes issues or trusts GitHub to re-grade  (Status: Accepted)
- Context: a terminal outcome needs an audit trail back on GitHub, but a CI run is
  not an issue to close.
- Decision: `report()` posts a grader-receipt comment to the run's commit (or its
  PR) via the injected runner; no issue mutation; no status flip.
- Rejected: closing/commenting an issue (wrong object); flipping the check status
  (trusting/forging CI state).
- Consequences: #8 grades the write-back target and that no issue is mutated.

### D-6: Reuse the existing reconcile + source_syncs posture; no new store  (Status: Accepted)
- Context: "failed listing != disappearance" already exists at runtime and storage
  (00048).
- Decision: the CI source flows through `sync_work_source` / `reconcile_live_runs`
  unchanged; a failed listing marks nothing disappeared.
- Rejected: a CI-specific store or reconcile path (duplicates 00048).
- Consequences: #6 grades the inherited posture for the CI source.

### D-7: Standalone single-source scope; multi-source (issues + CI together) is out of scope  (Status: Accepted)
- Context: `build_work_source(policy)` builds one source from `[source] kind`;
  aggregating multiple sources is a separate policy surface.
- Decision: ship the CI source standalone behind one `[source] kind`; do not add
  multi-source policy here.
- Rejected: multi-source policy now (separate design — precedence, dedup across
  sources, config shape).
- Consequences: keeps the spec right-sized; the `WorkGraphBuilder` aggregation seam
  already supports future multi-source composition without this spec.

## Open Questions (accepted gaps)
None. Every criterion lowers to a `command` grader over the orchestrator suite with
a fake `gh` runner. The exact `gh` invocation for "failed CI runs" (e.g.
`gh run list --status failure` vs a check-runs API call) and the precise write-back
call are implementation choices for /fw-plan — the spec grades the behavior (failed
runs become graded items; receipts are written back), not the path.

## Next Steps
Run `/fw-plan 00052-github-ci-failure-source` to compile these criteria into
flywheel tasks and graders.
