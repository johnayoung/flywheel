# Feature: PR Review Threads as Work

## Outcome
An unresolved GitHub pull-request review thread becomes a graded flywheel
WorkItem: the orchestrator drives an agent to address the reviewer's request,
and "done" is decided by the operator's `[defaults.graders]` run out-of-band
(plus the spec-00050/00051 held-out landing gate) — never by whether the
thread is marked resolved. Terminal outcomes leave a receipt as a PR comment;
no thread is ever resolved by the harness.

## Background
This is the second deferred GitHub-sources thread (after 00052's CI failures),
and the one the program split out precisely because its grade is subjective: a
review comment ("please add a test", "rename for clarity") is not a check.
The tacit trap a literal agent would exploit: a PR review thread already
carries a tempting "success" signal — `isResolved` — and resolving the thread
is the cheapest possible fake of "addressed." So resolution status is treated
exactly as 00052 treats the GitHub check status: it is an INPUT that selects
candidate threads, and is NEVER the verdict. The authoritative grade stays the
operator's declared graders, run by machinery the agent does not control, with
the held-out gate (00051) available for the operator to inject a per-thread
check keyed by task id. This keeps every work source on the one verified spine
the program has built.

## Scope
### In scope
- A new `github_review` work source: list a repo's open PRs' **unresolved**
  review threads via the `gh` CLI and compile each to a validated Task whose
  graders are the operator's default graders.
- Stable per-thread keying so a persistently-open thread is one work item
  across polls and across new replies.
- A `report()` that posts grader receipts as a PR comment and issues no
  thread-resolution mutation.
- `[source] kind = "github_review"` policy parsing + registry wiring +
  builder, mirroring the shipped `directory`/`github`/`github_ci` kinds.

### Out of scope
- Compiling the review comment text into a rubric/structured grader (rejected
  trust surface — see D-4). The grade is the operator's default graders.
- In-thread reply mutations and any thread-state mutation (D-5).
- PR-level review summaries or general PR comments that are not unresolved
  threads; auto-filtering on `isOutdated` beyond the `isResolved` gate.
- Changes to the issue (`github`) or CI (`github_ci`) sources.
- Pagination past one bounded listing page — the source reads a bounded page
  and logs when it truncates (the deferred issue-source-hardening thread owns
  full pagination; this source must not silently read truncation as "no work").

### Must not regress
- The shipped `directory`, `github`, and `github_ci` sources and their tests.
- The execute-time held-out landing gate (00050/00051).
- The full orchestrator + core suite stays green.

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its
grader type, visibility, the exact check, and the gaming move it forecloses.

1. When `list_work` runs and the repo has one unresolved and one resolved
   review thread, exactly one WorkItem is emitted, for the unresolved thread,
   and its task graders equal the operator's default graders. [command | held-out]
   verify: inject a GhRunner returning a GraphQL payload with one
   `isResolved:false` and one `isResolved:true` thread; assert
   `len(items)==1`, `items[0].task.graders == default_graders`, and
   `items[0].task.id` starts with `prc-`.
   defends against: emitting resolved threads as work; sourcing graders from
   anything other than the operator's declared policy.

2. If an unresolved review thread resolves to zero graders (no default grader
   policy), then no WorkItem is emitted for it and the skip is logged.
   [command | visible]
   verify: source built with `default_graders=()`, an unresolved thread
   present; assert `list_work()==[]` and the injected log received one skip
   line naming the item.
   defends against: running an ungated task graded by a fabricated/empty
   check; silently dropping a thread with no trace.

3. When the same review thread is listed across polls — including after a new
   reply is appended to it — the emitted WorkItem id is byte-identical, and a
   different thread yields a different id. [command | held-out]
   verify: two payloads for the same thread node id (the second adds a reply
   comment) yield equal `task.id`; a payload with a different thread node id
   yields a different `task.id`.
   defends against: per-comment / per-databaseId keying that re-queues the
   same thread as a new item on every reply (unbounded duplicates).

4. When a review thread gains a new comment between polls, the emitted
   WorkItem's `source_version` changes while its id stays identical.
   [command | visible]
   verify: same thread node id, second payload adds a reply; assert
   `source_version` differs and `task.id` is identical.
   defends against: a stale change-token that never reflects new reviewer
   input, so a re-opened concern is never re-driven.

5. The authoritative pass/fail for a review-thread item is the operator's
   default graders run out-of-band (plus the 00050 held-out gate); the source
   reads `isResolved` only to select candidates and derives no grader from it,
   so an unresolved thread whose default graders pass still reaches DONE.
   [command | held-out]
   verify: the compiled Task carries no grader whose value derives from
   resolution state; in the loop-integration test, an unresolved thread with
   passing default graders is driven to `Status.DONE` without any resolve
   call.
   defends against: treating reviewer "resolve" as the success signal — the
   check an agent could trip by resolving the thread instead of fixing it.

6. If the `gh` review-thread listing fails or returns malformed output, then
   `list_work` raises `WorkSourceError` and never returns an empty list.
   [command | held-out]
   verify: a runner returning invalid JSON, a non-object payload, a thread
   missing its required id, or a non-zero `gh` exit each raises
   `WorkSourceError`.
   defends against: a parse break or auth/rate-limit failure masquerading as
   "every thread is resolved" (a falsely green repo) so nothing is queued.

7. When the orchestrator finishes a review-thread run, `report()` posts the
   run's grader receipts as a PR comment and issues no thread-resolution
   mutation. [command | held-out]
   verify: inject a runner; after `report()`, assert one PR-comment call
   carrying the receipts body and the PR number parsed from `source_ref`;
   assert NO runner call contains a `resolveReviewThread` mutation.
   defends against: report closing/resolving the thread — flipping the exact
   state that must stay untrusted.

8. When `source.kind = "github_review"` with a `source.repo` is loaded,
   `load_policy` resolves `source_kind == "github_review"` and
   `build_work_source` returns a `GithubReviewWorkSource` bound to that repo
   and the operator's default graders. [command | visible]
   verify: `load_policy` on a `[source] kind="github_review"` + repo toml
   yields `source_kind=="github_review"`; `build_work_source(policy)` returns a
   `GithubReviewWorkSource` whose repo and graders match the policy.
   defends against: an unknown/typo'd kind silently coercing to `directory`;
   building a source detached from policy graders.

9. If `source.kind = "github_review"` is declared without `source.repo`, then
   `load_policy` raises `PolicyError` naming the missing `repo` key.
   [command | visible]
   verify: a toml with the kind but no repo raises `PolicyError` whose message
   mentions `repo`.
   defends against: a half-configured source that lists nothing and reads as
   "no work."

10. The compiled review-thread Task's context carries the full thread (every
    comment body with its author) and the thread/PR URLs. [command | visible]
    verify: assert `task.context.notes` contains each comment body and author
    login; assert `task.context.references` contains the thread URL.
    defends against: a goal-only task that strips the reviewer's specifics, so
    any diff could be claimed to "address" it.

Verification surface: unchanged. This feature adds a new adapter module plus
additive registry/policy entries; it changes no test, CI, fixture, grader, or
typecheck machinery. Standing DoD inherited by every task: the existing
orchestrator + core suite still runs and still passes after the change, and no
existing check is relaxed, removed, or skipped.

## Decomposition Hint (for /fw-plan)
- Layer adapter (`flywheel_orchestrator._github_review`): satisfies #1, #2,
  #3, #4, #6, #7, #10. A `GithubReviewWorkSource(*, repo, default_graders=(),
  runner=None, log=None)` with `source_kind="github_review"`, GraphQL listing,
  per-thread compilation, stable keying, grader-less skip, malformed→error,
  and a PR-comment `report()` that never resolves.
- Layer policy/registry (`_policy`, `_source_registry`): satisfies #5, #8, #9;
  depends on the adapter. Add `github_review` to `_SOURCE_KINDS`, parse
  `source.repo`, add `WorkPolicy.github_review_repo`, register the
  `github_review` kind → `build_github_review_source`, and prove end-to-end
  that an unresolved thread is driven to DONE by default graders with no
  resolve call (the #5 composition).
Shared invariants both layers assert against: `source_kind == "github_review"`;
`source_ref` format `"{repo}#{pr}#{thread_id}"` (D-6); the stable id prefix
`prc-`.

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-1: Separate adapter module, not an extension of the issue/CI sources  (Status: Accepted)
- Context: review threads have a different listing surface (GraphQL,
  per-PR-per-thread), a different stable key (thread node id), and a different
  write-back (PR comment, never resolve) than labeled issues or CI runs.
  Decision: ship a third standalone adapter `_github_review.py`
  (`GithubReviewWorkSource`) behind its own `[source] kind`, reusing only the
  `GhRunner` seam and `_format_report_body`.
- Rejected: subclassing `GithubWorkSource`/`GithubCiWorkSource` (couples three
  unrelated listing/keying/report shapes; 00052 D-1 set this precedent).
  Consequences: a little duplication of the JSON-decode/skip/error scaffold,
  bought back as isolation — each source fails independently.

### D-2: List via `gh api graphql` because resolution state is GraphQL-only  (Status: Accepted)
- Context: the `isResolved` thread state the candidate filter depends on is not
  exposed by the REST review-comments endpoint; it lives on
  `repository.pullRequest.reviewThreads`. Decision: list open PRs and their
  review threads with a single bounded `gh api graphql` query (one page),
  filter to `isResolved == false`.
- Rejected: REST `/pulls/{n}/comments` (no thread resolution; would force
  re-grouping comments into threads heuristically). Consequences: one bounded
  page only — when the page truncates, the source logs it rather than reading
  truncation as "no work" (full pagination is the deferred issue-hardening
  thread's job).

### D-3: Stable id keyed on the thread's GraphQL node id  (Status: Accepted)
- Context: a thread accumulates replies; per-reply keying would re-queue the
  same concern endlessly. The thread's GraphQL node id is stable across polls
  and across appended comments. Decision: `id = prc-<sha256(thread_node_id)[:16]>`
  — one thread is one work item.
- Rejected: keying on a comment databaseId or the comment count (advances on
  every reply → unbounded duplicates, the same trap 00052 D-3 avoided).
  Consequences: editing the thread never changes the id; freshness is carried
  by `source_version` (D-6) instead.

### D-4: Grade = operator default graders out-of-band + held-out gate; resolution is a filter, never the verdict  (Status: Accepted)
- Context: a review comment is subjective and not itself a check; the obvious
  but wrong move is to grade by `isResolved` or to compile the comment into a
  rubric. Decision: the grade is exactly the operator's `[defaults.graders]`
  run out-of-band, with the 00050/00051 held-out landing gate available for a
  per-thread operator-registered check keyed by the `prc-<digest>` task id. A
  thread with zero resolved graders is not runnable → skip + log.
- Rejected: (a) grading by thread resolution (the agent fakes it by resolving);
  (b) auto-compiling the comment into a rubric grader (a new comment→rubric
  trust surface whose own correctness the program has no machinery to verify).
  Consequences (honest limit, mirrors 00052 D-2): the default graders may not
  capture the reviewer's specific subjective concern. The out-of-band-ness is
  proven; subjective-intent capture is the operator's to align (tune defaults
  and/or register a held-out grader for that thread). It is never auto-derived.

### D-5: `report()` posts a PR comment and issues no thread mutation  (Status: Accepted)
- Context: the receipt needs a durable home on GitHub, and the anti-hack
  property "the harness never resolves a thread" must be structurally
  guaranteed. Decision: `report()` posts the receipts via `gh pr comment` on
  the PR parsed from `source_ref`, referencing the thread; it never calls any
  resolve/reply mutation.
- Rejected: an in-thread GraphQL reply via `addPullRequestReviewThreadReply`
  (more contextual, but adjacent in the API to `resolveReviewThread` — a
  PR comment makes accidental resolution impossible by construction).
  Consequences: receipts land at PR granularity, with the thread referenced in
  the body; the operator maps the `prc-` task id back to the thread.

### D-6: `source_ref = "{repo}#{pr}#{thread_id}"` round-trips report context  (Status: Accepted)
- Context: `WorkReport` carries only `source_ref` from the adapter, not
  `source_url`/`source_version`; `report()` needs both the PR number (to post)
  and the thread reference (for the body). Decision: encode both in
  `source_ref` as `owner/repo#<pr_number>#<thread_node_id>`, parsed back on the
  report path. `source_version` = `sha256(comment bodies + isResolved +
  isOutdated + latest comment timestamp)`; `source_url` = the thread URL.
- Rejected: PR-only `source_ref` (loses the thread reference in the receipt).
  Consequences: `report()` partitions `source_ref` into repo / pr / thread_id;
  GraphQL node ids contain no `#`, so the delimiter is unambiguous.

## Open Questions (accepted gaps)
None blocking. The subjective-intent gap in D-4 is an accepted, recorded limit
(operator aligns default graders / registers a held-out grader), not an
un-gradeable criterion.

## Next Steps
Run `/fw-plan 00053-pr-review-thread-source` to compile these criteria into
flywheel tasks and graders.
