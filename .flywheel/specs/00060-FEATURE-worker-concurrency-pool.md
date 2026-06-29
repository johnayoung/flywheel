# Feature: Worker concurrency pool

## Outcome
A single `flywheel worker` invocation can drain the task queue with up to N
tasks executing **simultaneously**, where N comes from a `[worker] concurrency`
setting in `flywheel.toml` (overridable per-run by `--concurrency`). When the
setting is absent or `1`, behavior is byte-for-byte identical to today's single
serial worker. Raising N to M (with at least M independent ready tasks) results
in M tasks observably in the in-progress state at the same instant, while every
existing safety invariant — exactly-once execution, serialized landing, clean
shutdown — continues to hold under the pool.

## Background
The scheduler already computes the full parallelizable frontier every pass:
`WorkGraph.ready_set()` returns *every* currently-runnable task (prereqs DONE,
own state eligible, capabilities fit), priority-ordered, and the per-task
lease/claim layer (hardened by commit 4dc477b, which closed the fresh-selection
double-claim TOCTOU) makes concurrent execution *safe*. What is missing is purely
the dispatch switch: `orchestrate()` consumes only `ready[0]` per pass and a
`flywheel worker` process wraps exactly one of those, so today the only way to
run two tasks at once is to launch two worker processes by hand. The tacit
requirement surfaced in the interview: concurrency is real CPU/token spend (each
worker is its own agent/SDK session + sandbox), so it must be **opt-in and
explicit**, never a silent default that changes every existing repo's load. The
chosen shape is a **process pool** (one OS process per concurrent task), matching
the codebase's existing "parallelism is per-process" design and reusing the
group-kill supervisor model already in the tree.

## Scope
### In scope
- A `[worker] concurrency = N` config field and a `--concurrency N` CLI flag for
  `flywheel worker`; the flag overrides the config; default is `1`.
- A worker pool supervisor that runs up to N concurrent single-task workers from
  one `flywheel worker` invocation, each with a distinct worker id and its own
  worktree, against one shared store.
- Restart-on-crash: a dead pool member is respawned so the live pool returns to
  size N; its in-flight task's lease expires and is reclaimed by a live worker.
- Preservation, under the pool, of the existing exactly-once, serialized-landing,
  and orphan-free-shutdown guarantees.

### Out of scope
- **Parallel landing.** Landings (FF-merge onto the base) stay serialized through
  the existing repo merge-flock; only *execution* is parallelized.
- **In-process async fan-out** (running N `orchestrate()` coroutines in one
  process) — the rejected alternative to the process pool (see D-5).
- **Auto / CPU-derived default concurrency** — the default stays `1` (see D-1).
- A `[autopilot] concurrency` knob or any change to the autopilot refill daemon,
  which emits tasks and never executes them.
- Any change to the tier model, scoring, work sources, or the grader/landing rules
  themselves beyond exercising them concurrently.

### Must not regress
- Single-worker behavior when `concurrency` is unset or `1` (the existing
  orchestrator/worker suite stays green, unchanged).
- Exactly-once execution: no task is ever run or landed by two workers.
- Submit-time re-verification: nothing lands that was not verified against the
  exact base it lands on.
- Orphan-free shutdown (the group-kill guarantee from commit 36a0622).

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its grader
type, visibility, the exact check, and the gaming move it forecloses. `/fw-plan`
lowers each one to a command / transcript / rubric / manual grader.

1. When `concurrency` is unset (or `1`), the worker has at most one task in the
   in-progress state at any observed instant across a multi-task drain. [command | visible]
   verify: drive a queue of >=3 independent tasks with concurrency unset; sample the
   store's in-progress count throughout the drain and assert the maximum observed is 1;
   the existing orchestrator/worker test suite passes unchanged.
   defends against: an implementation that always pools (silently changing the default),
   or that reports "default 1" while dispatching more than one.

2. When `[worker] concurrency = N` with N>=2 and at least N independent ready tasks
   exist, the worker reaches a state where N tasks are simultaneously in-progress. [command | held-out]
   verify: N=3 with 3 prereq-free tasks whose invoke blocks on a shared barrier that
   releases only once 3 tasks are concurrently in-progress (within a timeout); the drive
   completing proves true overlap, not N serial runs.
   defends against: spawning N processes that serialize on a shared lock (no real overlap),
   or a test that asserts "N processes exist" instead of "N tasks overlap in execution".

3. While running at concurrency N, at most N tasks are in the in-progress state at
   any observed instant. [command | held-out]
   verify: N=2 with 5 independent ready tasks; sample the in-progress count across the
   whole drain and assert the maximum observed is exactly 2 and never exceeds it.
   defends against: an unbounded fan-out that ignores N (runaway process / token spend).

4. While running at concurrency N over M>N independent tasks, each task is executed
   to exactly one DONE run and the union of executed tasks covers all M (none twice). [command | held-out]
   verify: a deterministic exactly-once stress test (the pool analog of
   test_fresh_selection_rechecks_terminal_state_under_claim) with induced overlap; assert
   per-task run count == 1 and total runs == M.
   defends against: a pool that double-claims/double-lands under contention (the TOCTOU
   class fixed in 4dc477b regressing), or tasks too fast to overlap masking the race.

5. When `--concurrency K` is passed, the effective pool size is K regardless of the
   `[worker] concurrency` config value. [command | visible]
   verify: config sets concurrency=3, run with `--concurrency 1`, and assert at most one
   task is in-progress at any instant (flag wins); separately config=1, `--concurrency 3`,
   assert 3 reach in-progress concurrently.
   defends against: the flag being silently ignored, or config taking precedence over the
   explicit per-run override.

6. If `concurrency` resolves to a value < 1 (e.g. `0`, negative, or non-integer),
   then the worker exits non-zero with an error naming the setting and claims no task. [command | visible]
   verify: `flywheel worker --concurrency 0` exits non-zero, the message names the
   concurrency setting, and the store shows no claim/run created.
   defends against: silently clamping 0 to 1 or treating it as "infinite", or proceeding
   with a nonsensical pool size.

7. When `concurrency` N exceeds the number of currently-ready tasks, the surplus
   workers idle without error and without claiming any task more than once. [command | visible]
   verify: N=4 with a single ready task; assert the task reaches exactly one DONE run, the
   worker exits 0 (in `--once` mode), and no task is double-claimed; assert no busy-spin
   (bounded claim attempts / a transcript bound on idle polling).
   defends against: surplus workers erroring, crashing, double-claiming, or hot-looping on
   an empty ready set.

8. If a pool member process dies mid-task, then the supervisor restores the pool to
   N live workers and the interrupted task still reaches exactly one DONE run. [command | held-out]
   verify: with N>=2, force one pool member to die mid-task (injected hard exit); assert the
   pool returns to N live members and the interrupted task is reclaimed and completes with
   exactly one DONE run (not zero, not two).
   defends against: a crashed worker permanently shrinking the pool, its task being orphaned
   and never reclaimed, or the task being double-run on reclaim.

9. When the worker pool is stopped (SIGTERM / console quit), then no pool-member
   process and no agent/MCP child of one survives the shutdown. [command | held-out]
   verify: start a pool of N workers each with a SIGTERM-ignoring child + grandchild
   (the existing supervisor-shutdown test pattern), stop the pool, and assert zero
   descendant processes remain after the stop timeout.
   defends against: the orphan-leak bug class (36a0622) reappearing at the pool layer — a
   stop that signals only the pool parent and leaks N worker subtrees to init.

10. While multiple workers complete near-simultaneously, each task lands only after
    re-verifying against the exact base it lands on, and the final base contains every
    landed task's change with no partial/interleaved merge. [command | held-out]
    verify: N=2 with two tasks that each commit a distinct change graded by a command;
    assert both land, the post-drain base contains both changes, and the full verification
    suite is green on the landed base (proving submit-time re-verification ran).
    defends against: concurrent landings racing the merge (lost update / partial merge), or
    submit-time re-verification being skipped under contention.

Verification surface: unchanged. This feature adds a concurrency mechanism; it does
not modify the tests, grading commands, CI config, fixtures, or any machinery that
decides whether a change is correct. The existing suite is the regression oracle for
criteria 1 and 4; no existing check is relaxed, removed, or skipped.

## Decomposition Hint (for /fw-plan)
Split along these layers; chain with prerequisites so no slice inherits a red suite.
- Layer config: parse `[worker] concurrency` and resolve it against `--concurrency`
  (flag wins), with `< 1` rejected. Satisfies #5, #6; contributes the `concurrency`
  field that every later layer reads.
- Layer pool-supervisor: spawn and supervise up to N single-task workers from one
  invocation, each a distinct worker id + own worktree, with restart-on-crash and
  group-kill shutdown. Satisfies #2, #3, #7, #8, #9. Depends on config.
- Layer concurrency-safety: prove the existing lease/claim + landing guarantees hold
  under the pool. Satisfies #4, #10, and the N=1 back-compat half of #1. Depends on
  pool-supervisor. (Mostly already true post-4dc477b; this slice is the held-out proofs.)
Shared invariants multiple layers assert against: the resolved `concurrency` value
(config field + flag precedence), and per-pool-member worker-id uniqueness (so claims
attribute to distinct workers — load-bearing for the exactly-once and crash-reclaim
proofs).

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-1: Config field + flag, default 1 (opt-in)  (Status: Accepted)
- Context: concurrency is real CPU/token spend (one agent/SDK session + sandbox per
  worker); a silent parallel default would change every existing repo's load. |
  Decision: `[worker] concurrency` in `flywheel.toml`, `--concurrency` overrides
  per-run, default `1` (today's behavior byte-for-byte until opted in).
- Rejected: flag-only (the fw console couldn't inherit it from config); config-only
  (no per-drain override); auto/CPU-derived default (changes behavior for everyone,
  unexpected load spike). | Consequences: parallelism is never on by accident; the
  operator must deliberately set it.

### D-2: Crash policy is restart-and-reclaim  (Status: Accepted)
- Context: an unattended daemon should hold its configured throughput; a pool that
  decays toward 1 over hours is a silent regression. | Decision: a dead pool member
  is respawned to restore size N; its in-flight task's lease expires and a live worker
  reclaims it (reusing the existing lease-reclaim path).
- Rejected: continue-with-fewer (pool silently erodes); fatal-on-any-crash (one flaky
  task kills all in-flight work — too brittle). | Consequences: a pathological
  crash-looping task can be retried by successive workers; bounded by the existing
  max-retries / lease machinery.

### D-3: Concurrent execution, serialized landing  (Status: Accepted)
- Context: execution is where wall-clock goes; landing is fast and already protected
  by the repo merge-flock + submit-time rebase/re-verify. | Decision: parallelize
  execution only; landings stay serialized through the existing flock, each
  re-verifying against the exact base it lands on.
- Rejected: parallel landing (needs new conflict-resolution + re-verification
  machinery for marginal finish-line gain). | Consequences: under heavy concurrency a
  burst of simultaneous completions queues briefly at the landing step; acceptable.

### D-4: A concurrency value < 1 is a hard config error  (Status: Accepted)
- Context: `0`/negative are ambiguous (infinite? disabled? one?) and a silent clamp
  hides operator mistakes. | Decision: resolve-time validation rejects `< 1` with a
  non-zero exit and an error naming the setting; no task is claimed.
- Rejected: clamp-to-1 (hides typos); treat 0 as unbounded (runaway spend). |
  Consequences: a malformed value fails fast and loud rather than running surprisingly.

### D-5: Process pool, not in-process async fan-out  (Status: Accepted)
- Context: the codebase's stated model is "parallelism is per-process," with leases +
  merge-flock as the coordination primitives, and a group-kill supervisor already in
  the tree. | Decision: one OS process per concurrent task, supervised from a single
  `flywheel worker` invocation.
- Rejected: N `orchestrate()` coroutines in one process (lighter, but shares one
  process/event loop and diverges from the established isolation model). |
  Consequences: higher per-worker memory/process overhead, bought for stronger
  isolation, crash containment, and reuse of the existing supervisor.

## Open Questions (accepted gaps)
None. Every criterion lowers to a `command` grader; the held-out proofs (#2, #3, #4,
#8, #9, #10) are the authoritative grades and assert observable store/process state the
implementing agent cannot pre-compute from known inputs.

## Next Steps
Run `/fw-plan 00060-FEATURE-worker-concurrency-pool` to compile these criteria into
flywheel tasks and graders.
