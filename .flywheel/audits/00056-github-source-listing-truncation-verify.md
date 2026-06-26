# 00056 github-source-listing-truncation — fw-verify audit

Blind held-out discrimination proofs for spec
`.flywheel/specs/00056-github-source-listing-truncation.md`, run AFTER execute
against the shipped code on `main` (commits `1a13fc2` issue source, `b2b8791`
CI + review sources). Mirrors the 00055 method: blind oracles authored from the
declared public contract only, a synthesized-mutant discrimination gate, and a
real-grade pass against the shipped sources.

Verification scratch is git-ignored under `.flywheel/verification/00056/`
(four oracles, `mutants_check.py`, `probes_real_grade.py`); only this audit is
durable. The permanent regression guard is the tasks' own committed tests in
`packages/flywheel-orchestrator/tests/test_github_source.py`,
`test_github_ci_source.py`, and `test_github_review_source.py`, run by the
command graders and CI.

## Result

- **4 blind held-out oracles authored** (A/B/C/D), each by a fresh fenced
  subagent that read only its one criterion + the public `GithubWorkSource`
  contract — never the new truncation code (`emit_truncation_warning`, the
  `len(...) == _LIST_LIMIT` trigger, or the exact log string). All ADMITTED.
- **Discrimination gate: 4/4 mutant kills, shipped source passes all 4**
  (`mutants_check.py`), flake-screened run-twice via pytest (identical
  verdicts, 5 passed each run).
- **Uniformity real-grade**: the same four behaviors real-graded against the
  shipped `GithubCiWorkSource` and `GithubReviewWorkSource` (they share the one
  `emit_truncation_warning` seam): **6/6 probes PASS** (`probes_real_grade.py`).
- Integrated `main` green: ruff 0, pyright 0, **2054 passed**.

## Routing

| Criterion | Route | Why |
| --- | --- | --- |
| #1 capped page -> truncation warning | AUTHOR oracle A | behavior; a silent drop passes a "listing returned" check |
| #2 under-cap -> no warning | AUTHOR oracle B | behavior; an unconditional warning is constant noise |
| #3 page unchanged with/without sink | AUTHOR oracle C | behavior; a logging path that trims/reorders changes dispatched work |
| #4 failed gh -> WorkSourceError, never [] | AUTHOR oracle D | behavior; fail-open reads as disappearance (the anti-hack) |
| #5 sink-only, no stdout leak | SKIP (visible) | tagged `visible`; `log=None` silence is graded by the committed suite |

## Oracles and discrimination proofs

All four blind oracles expose `verify(make_source)` where
`make_source(runner, log)` builds a source exposing `.list_work()`. The default
factory builds the **shipped** `GithubWorkSource`, so "the correct reference
passes" IS the real-grade against shipped code. Each mutant is a `GithubWorkSource`
subclass injecting exactly the gaming move its criterion defends against.

### A — capped page warns (`A-capped-warns/test_capped_warns.py`)
- **Asserts**: an injected runner returning exactly 200 runnable issues makes
  `list_work` log a line containing "truncat" (case-insensitive), and the full
  200-item page is still returned.
- **Mutant `NeverWarnSource`** (filters truncation lines from the sink): the
  capped page is listed silently. **KILLED** — no "truncat" line on the sink.
- Real-grade: shipped source PASSES (warns at the cap, returns all 200).

### B — under-cap stays silent (`B-undercap-silent/test_undercap_silent.py`)
- **Asserts**: an injected runner returning 3 issues (< cap) logs NO line
  containing "truncat".
- **Mutant `AlwaysWarnSource`** (emits a truncation line every pass): the
  signal becomes constant noise. **KILLED** — a "truncat" line appears below
  the cap.
- Real-grade: shipped source PASSES (silent below the cap).

### C — items invariant (`C-items-invariant/test_items_invariant.py`)
- **Asserts**: over the same 200-item capped payload, the WorkItem id sequence
  returned WITH a log sink equals the sequence returned without one (same ids,
  order, count).
- **Mutant `DropWhenLoggingSource`** (logging path drops the last item): the
  with-sink page is 199, the no-sink page is 200. **KILLED** — lengths differ.
- Real-grade: shipped source PASSES (logging is a pure side channel).

### D — fail-closed (`D-fail-closed/test_fail_closed.py`)
- **Asserts**: a runner that raises `WorkSourceError` (a failed `gh` call)
  makes `list_work` propagate `WorkSourceError` and never return `[]`.
- **Mutant `FailOpenSource`** (catches `WorkSourceError`, returns `[]`):
  **KILLED** — `list_work` returns `[]` instead of raising, tripping the
  oracle's non-raising guard.
- Real-grade: shipped source PASSES (fail-closed).

```
$ uv run python .flywheel/verification/00056/mutants_check.py
[A-capped-warns]   KILL NeverWarnSource: expected a truncation warning on the log sink ...
[B-undercap-silent]KILL AlwaysWarnSource: under-cap listing (3 < 200) must emit no truncation warning ...
[C-items-invariant]KILL DropWhenLoggingSource: ... with log -> 199 items, without log -> 200 ...
[D-fail-closed]    KILL FailOpenSource: list_work() must raise WorkSourceError ... but it returned: []
Killed 4/4 mutants; shipped GithubWorkSource passed all 4 oracles (real-grade).
```

## Uniformity real-grade (CI + review share the emitter)

The blind oracles discriminate on the issue source — the headline REST
silent-drop case. The CI and review sources route the SAME behaviors through the
one shared `emit_truncation_warning` seam, so they are real-graded (not blind
discrimination) against shipped code:

```
$ uv run python .flywheel/verification/00056/probes_real_grade.py
CI capped(200)->warns: PASS          review truncated-axis->warns: PASS
CI undercap(3)->silent: PASS         review no-next-page->silent: PASS
CI fail-closed: PASS                 review fail-closed: PASS
6/6 probes PASS
```

CI truncation triggers on the raw run count at the cap (before the
`(workflow, branch)` dedup); review triggers on any GraphQL axis whose
`pageInfo.hasNextPage` is true (the probe drives the open-PRs axis).

## Honest limits

- **Discrimination is proven on the issue source.** CI (identical REST
  `count == cap` mechanism) and review (GraphQL `hasNextPage`) are real-graded
  against shipped code, not gated by a blind synthesized-mutant kill. Their
  discrimination is carried by the shared `emit_truncation_warning` seam that
  oracles A/B/C/D kill mutants on; a separate-mechanism mutant for review would
  amount to reimplementing the GraphQL parse. This mirrors 00055's split (store
  oracles blind-discriminate; loop/Postgres real-graded).
- **#5 (no stdout leak) is `visible`, routed SKIP.** The committed suite's
  `log=None` constructions and the side-channel `if log is None: return` guard
  cover it; it is not a held-out discrimination target.
- **D-4's at-cap heuristic.** A board with exactly 200 matching items yields a
  conservative false-positive "may have more" warning (REST exposes no
  `hasNextPage`). This is the accepted trade (over-report beats silent drop) and
  is not a defect the oracles need to kill; criterion #2 still forbids warning
  below the cap, which oracle B enforces.
- **Registration deferred.** No `[held_out] root` is set in `flywheel.toml`
  (same as 00053/00054/00055), so the admitted oracles are not wired as an
  execute-time held-out gate. The proof is recorded here; the durable
  regression guard stays the tasks' committed suites.

## Provenance

- Spec: `.flywheel/specs/00056-github-source-listing-truncation.md` (5 criteria,
  4 held-out).
- Tasks (archived): `.flywheel/tasks/archive/github-source-listing-truncation/`
  (`01-truncation-warning-issue-source`, `02-truncation-warning-ci-review-sources`).
- Shipped: `1a13fc2` (issue source + shared `emit_truncation_warning`),
  `b2b8791` (CI + review routed through the emitter).
- Verify scratch (git-ignored): `.flywheel/verification/00056/`.
