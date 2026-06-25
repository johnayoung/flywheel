# Feature: GitHub Source Listing Truncation Transparency

## Outcome
All three GitHub work sources (labeled issues, CI failures, unresolved PR review
threads) make bounded-page truncation an explicit, uniform log signal instead of
silently dropping the overflow. Today the issue source (`gh issue list --limit
200`) and the CI source (`gh run list --limit 200`) fetch one bounded page and
drop anything past the cap with no warning; only the review source logs when its
GraphQL listing is truncated. After this change, whenever any of the three
sources fetches a page that fills its single-page cap (and therefore cannot rule
out more matching work), it emits a truncation warning naming the source and
signaling that some items were not read this pass — through the same warning
mechanism, with the issue and CI sources gaining the transparency the review
source already has. The fail-closed posture on `gh` failures (auth, rate limit,
API error) is preserved unchanged: a failed listing still raises, never returns
an empty or partial list that downstream could misread as "the work disappeared."

## Background
A flywheel operator points a source at a busy board; the board has more than 200
open matching issues (or 200 failed CI runs). The source silently returns the
first bounded page and the rest is invisible — there is no signal anywhere that
work was left unread, so the operator believes the queue is fully drained when it
is not. The review source already closed this gap for itself (it logs
`[github_review] ... listing truncated at one page; some threads were not read
this pass` at each GraphQL axis), but issues and CI did not get the same
treatment, leaving an asymmetric, silent hole on the two highest-volume sources.

The tacit requirement a literal agent would miss: this is the listing-layer form
of flywheel's load-bearing "a failed listing is not a disappearance" invariant
(`reconcile_live_runs`). The danger in "handle rate limits / pagination
gracefully" is an agent that swallows a `gh` error or a truncated page and
returns `[]` (or a trimmed page) to look robust — which the reconcile loop reads
as "all that work vanished," reclaiming or closing live work. So truncation must
be *surfaced*, never *absorbed*, and an authentication/rate-limit failure must
stay a hard, fail-closed raise — never be downgraded to a quiet empty success.

## Scope
### In scope
- The issue source and the CI source emit a truncation warning to their existing
  log sink when their single bounded listing page comes back filled to the cap.
- The review source's existing per-axis truncation warning is preserved and
  routed through the same shared warning mechanism, so all three sources signal
  truncation uniformly (the same shape: names the source, says some items were
  not read this pass).
- A shared truncation-warning emitter reused by all three sources, living
  alongside the already-shared `gh` runner seam (`_default_runner` / `GhRunner` /
  `_format_report_body`).
- The fail-closed contract (any nonzero `gh` exit -> `WorkSourceError`, never a
  partial/empty success) is explicitly preserved and pinned by a held-out check.

### Out of scope
- Cursor / multi-page pagination that drains work past the cap in one pass
  (`gh --paginate`, GraphQL `endCursor` loops). Truncation is surfaced, not
  resolved; the overflow re-drains on the next scheduling pass. (D-1.)
- Rate-limit backoff/retry, or distinguishing a 429/secondary-limit from an auth
  failure inside the runner. The runner stays fail-closed and retry-free. (D-2.)
- Making the single-page cap (200 for the REST sources; 50/50/50 for the GraphQL
  review source) configurable, or changing its value. (D-5.)
- Eventual-consistency / list-lag (a just-created issue not yet returned by the
  API). That is a GitHub API property truncation transparency does not address.
- Any new `flywheel.toml` config surface, and any non-GitHub source
  (Linear/Slack are explicitly out of the program).

### Must not regress
- Each source's existing item compilation: stable ids (`gh-<n>` / `ci-<digest>`
  / `prc-<digest>`), spec-block / default-grader resolution, the per-item
  "skipping ... not runnable" log for grader-less items, and report posting.
- The review source's existing truncation logging at all three GraphQL axes
  (open PRs, review threads per PR, comments per thread).
- The fail-closed runner posture: a nonzero `gh` exit raises `WorkSourceError`;
  malformed JSON / wrong-shaped payloads raise; no source returns `[]` on error.
- `flywheel-core` purity and the orchestrator store schema are untouched (this is
  a source-adapter-only change; no schema bump, no store change).

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its grader
type, visibility, the exact check, and the gaming move it forecloses. `/fw-plan`
lowers each one to a command / transcript / rubric / manual grader.

1. When a GitHub work source's listing fills its single-page cap, that source
   emits a truncation warning to its log sink that names the source and signals
   some items were not read this pass. [command | held-out]
   verify: drive each of the three sources via an injected `gh` runner returning
   a capped listing — for the REST issue and CI sources a page of exactly the
   single-page limit of runnable items; for the GraphQL review source a payload
   whose `pageInfo.hasNextPage` is true at the open-PRs axis, the review-threads
   axis, and the comments axis. Capture the log sink; assert each source emits at
   least one warning that identifies that source and conveys that work was left
   unread this pass, and that the review source emits one per truncated axis.
   defends against: silently dropping overflow so an operator believes the queue
   is drained when the board exceeded the cap (the silent-drop hole on the issue
   and CI sources).

2. When a GitHub work source's listing comes back below its cap (no further
   page), that source emits no truncation warning. [command | held-out]
   verify: drive each of the three sources via an injected runner returning fewer
   items than the cap (and, for the review source, every `hasNextPage` false);
   capture the log sink; assert no truncation warning line is emitted by any
   source (per-item "not runnable" skip lines are unrelated and allowed).
   defends against: firing the warning unconditionally so the truncation signal
   is constant noise that trivially passes a "did it warn" check while telling
   the operator nothing.

3. When a source's listing is truncated, it still returns the full bounded page
   it fetched; the truncation warning is a side channel that does not drop,
   trim, reorder, or otherwise change the returned work items. [command | held-out]
   verify: for each source, run `list_work` over a capped payload twice — once
   with a log sink attached, once with no log sink — and assert the two returned
   `WorkItem` sequences are identical (same ids, same order, same count).
   defends against: "handling" truncation by trimming or reordering the page,
   silently changing which work is dispatched under the guise of a logging fix.

4. When the underlying `gh` invocation fails (nonzero exit: auth failure, rate
   limit, or API error), the source raises `WorkSourceError` and never returns
   an empty or partial list. [command | held-out]
   verify: inject a runner that simulates a failed `gh` call (raises
   `WorkSourceError`, as the real `_default_runner` does on nonzero exit); assert
   each source's `list_work` propagates `WorkSourceError` and does not return
   `[]`; separately assert the shipped `_default_runner` still raises on a
   nonzero exit and does not swallow it.
   defends against: downgrading a rate-limit/auth error to a quiet `[]` to look
   robust — which the reconcile loop reads as "all work disappeared," reclaiming
   or closing live work (failed-listing-is-not-disappearance, at the listing
   layer).

5. A source constructed without a log sink completes a truncated listing without
   error and writes nothing to stdout; the truncation warning is emitted only
   through the injected log sink. [command | visible]
   verify: construct each source with `log=None` over a capped payload; assert
   `list_work` returns the bounded page without raising and produces no captured
   stdout output.
   defends against: leaking warnings via `print`/stdout where the orchestrator's
   log routing cannot capture, route, or suppress them, and where a sink-less
   construction would crash on the new warning path.

Verification surface: unchanged. This feature changes three source adapters and a
shared helper; it does not touch the test/lint/typecheck/CI machinery, the
graders, the store schema, or any check that decides whether other changes are
correct.

## Decomposition Hint (for /fw-plan)
Two layers; the shared emitter is the root contract both consumers assert against.

- Layer A (shared emitter + issue source): satisfies #1/#2/#3/#5 for the issue
  source and #4 for the issue source. Introduces the single shared
  truncation-warning emitter (alongside `_default_runner` / `GhRunner` /
  `_format_report_body` in the issues module) and the issue source's full-page
  detection (a page returned at its single-page limit triggers the warning).
  Depends on nothing.
- Layer B (CI source + review source routed through the emitter): satisfies
  #1/#2/#3/#5 for the CI and review sources, #4 for both, and the review
  per-axis must-not-regress. The CI source gains full-page detection identical in
  shape to the issue source; the review source's three existing per-axis
  truncation calls are routed through the shared emitter so the signal shape is
  uniform. Depends on Layer A's emitter contract.

Shared invariants both layers assert against (name them so the slices move
together): the truncation-warning shape (a single log line that names the source
and states some items were not read this pass); the full-page trigger for the
REST sources (a returned page whose size equals the single-page cap); and the
preserved fail-closed runner contract (nonzero `gh` exit -> `WorkSourceError`,
never `[]`).

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-1: Surface truncation, do not paginate past the cap  (Status: Accepted)
- Context: the operator chose transparent truncation over real cursor pagination.
  The scheduling loop re-lists every pass, so overflow past the cap re-drains on
  the next pass; the gap is visibility, not eventual coverage. | Decision: each
  source logs when it truncates at one bounded page and returns that page
  unchanged; no `gh --paginate` / GraphQL `endCursor` loop is added.
- Rejected: full multi-page pagination — unbounded fetch per pass, heavier `gh`
  load, and a much larger test surface for marginal gain since the queue already
  re-drains the overflow.
- Consequences: a board larger than the cap is drained over multiple passes, not
  one; the per-pass truncation warning is the operator's signal that this is
  happening. Real pagination remains a clean future spec if volume demands it.

### D-2: Keep the runner fail-closed; no rate-limit backoff/retry  (Status: Accepted)
- Context: the operator chose to preserve the current uniform fail-closed posture
  over adding bounded backoff/retry. The reconcile loop already treats a failed
  listing as "not disappeared," so a rate-limited pass safely no-ops. | Decision:
  any nonzero `gh` exit (auth, rate limit, API error) raises `WorkSourceError` as
  today; no retry/backoff and no 429-vs-auth distinction is added in this spec.
- Rejected: swallowing the error and returning `[]` (the headline anti-hack —
  reads as disappearance, criterion #4 forecloses it); bounded backoff/retry
  inside the runner (adds non-deterministic timing/sleep to the seam and couples
  retry policy to a truncation-transparency spec).
- Consequences: under heavy use a rate-limited pass raises and the worker retries
  next pass; resilient backoff stays a separable future concern.

### D-3: One shared truncation-warning emitter reused by all three sources  (Status: Accepted)
- Context: the three sources already share the `gh` runner seam (`_default_runner`
  / `GhRunner` / `_format_report_body`), but only the review source logs
  truncation, with its own inline string. | Decision: introduce a single shared
  emitter that produces the truncation warning, used by all three sources so the
  signal shape is uniform; the review source's three per-axis calls route through
  it.
- Rejected: leaving each source to emit its own ad-hoc string (the status-quo
  asymmetry that produced the silent-drop hole on issues/CI); a per-source
  copy-paste of the review string (no convergence, drifts over time).
- Consequences: the review source's message generalizes (e.g. "threads" -> the
  uniform "items" wording). The old review string was never a graded contract
  (00053's verified criteria were stable-keying, fail-closed, never-resolves —
  not the truncation text), so the wording change is acceptable; criterion #1
  grades the warning's meaning, not its exact bytes.

### D-4: Full-page-at-cap is the REST truncation trigger (conservative)  (Status: Accepted)
- Context: `gh issue list` / `gh run list` with `--limit N` expose no
  `hasNextPage` signal; the only available truncation evidence is that the
  returned page came back filled to the cap. | Decision: a REST source treats a
  page whose size equals its single-page cap as possibly-truncated and emits the
  warning; the GraphQL review source uses its real `pageInfo.hasNextPage`.
- Rejected: requesting `limit + 1` and trimming to get exact detection — extra
  machinery for a "may have more" warning; not forbidden, an implementation may
  choose it (a page that would exceed the cap still satisfies criterion #1), but
  not required.
- Consequences: a board with exactly `cap` matching items yields a conservative
  false-positive warning ("may have more" when there is no more). That is the
  accepted trade: a warning that over-reports is strictly safer than a silent
  drop that under-reports, and criterion #2 still forbids warning below the cap.

### D-5: The single-page cap value is unchanged and stays non-configurable  (Status: Accepted)
- Context: the cap (200 REST; 50/50/50 GraphQL) is a pre-existing constant; this
  spec is about visibility of hitting it, not its value. | Decision: leave the
  cap as-is and add no config knob for it.
- Rejected: a `flywheel.toml` cap setting — config surface for a concern nobody
  raised; gold-plating beyond the requested transparency fix.
- Consequences: tuning the cap remains a code constant; if operators later need
  it configurable that is a separate, additive spec.

## Open Questions (accepted gaps)
None. Every criterion lowers to a `command` grader (pytest assertions over each
source driven by an injected `gh` runner returning canned capped / under-cap /
failing payloads), with the four highest-stakes criteria marked held-out for
fw-verify.

## Next Steps
Run `/fw-plan 00056-github-source-listing-truncation` to compile these criteria
into flywheel tasks and graders.
