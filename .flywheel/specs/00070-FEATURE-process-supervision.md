# Feature: Process supervision (respawn dead daemons under a bounded budget)

## Outcome
A dead long-running flywheel daemon — the autopilot intake loop or a
git-worktree worker, started from the operator console OR headless — is
**automatically respawned** by its supervisor under one shared, bounded
crash-loop budget (at most N respawns inside a rolling window), instead of being
observed-and-reported-dead with no restart. A daemon that keeps dying past that
budget stops being respawned and is left in a loud, queryable DEAD-after-budget
state rather than silently gone or silently crash-looping. A running daemon is
discoverable via a liveness record so a restart adopts the live one instead of
spawning a duplicate, and a single worker-pool slot that exhausts its budget is
retired/quarantined without group-killing the rest of the fleet.

## Background
Today every supervisor in the tree *watches* a daemon die and reports `DEAD`,
but nothing restarts it: the autopilot supervisor (`_autopilot_supervisor.py`)
and worker supervisor (`_worker_supervisor.py` — "no restart policy beyond an
explicit `start()`") both flip to a `DEAD` status and the dashboard just prints
"type '/autopilot start' to respawn." Headless runs (`flywheel autopilot` /
`python -m flywheel_orchestrator._autopilot_run`) have **no** supervisor object
at all — a crash ends them with nothing watching. Autopilot is also deliberately
not auto-started today because it writes the operator's base branch unattended;
that same property is *why* respawn must be bounded — an unbounded respawn would
crash-loop into the base branch. The autopilot supervisor writes no liveness
record, so a console-exit-detached daemon is unadoptable: the next console can
neither find nor adopt it and will declare it dead or spawn a duplicate. And the
worker pool's restart budget is `MAX_POOL_RESTARTS_PER_SLOT = 5` **lifetime per
slot, never decaying**, after which `_supervise_tick` sets `_stop_requested` and
the `finally: stop()` **group-kills every other live member** — one bad slot
takes down the whole healthy fleet.

The tacit requirement surfaced in the interview: the bounded budget is not a
nicety, it is the safety interlock. Because the autopilot worker lands the
operator's base branch with no human present, the only thing standing between a
boot-loop bug and an unbounded stream of unattended base-branch writes is the
crash-loop budget. The budget MUST be windowed (transient blips recover; a
persistent fault is contained), and exhausting it MUST be a loud, queryable
terminal state, not a silent stop. This phase is the outermost loop of a 7-phase
program and is only safe because the inner loops were already made non-crash-
looping by the earlier phases (P1 containment, P2 deadlines, P3 transient
resilience); see Decisions D-A.

## Scope
### In scope
- A single shared **supervision policy** — respawn-on-death + a windowed
  crash-loop budget (≤ N respawns per rolling window) + a liveness record —
  consumed identically by the autopilot supervisor, the worker supervisor, and
  the worker pool, so all three sites enforce one policy rather than three
  divergent ones.
- Auto-respawn: a supervised daemon observed dead inside budget is respawned
  without an operator command.
- A loud, queryable **DEAD-after-budget** terminal state: once a daemon exceeds
  N deaths in the window, the supervisor stops respawning and reports a distinct
  status that an operator (or a status query) can read.
- A **headless supervised entrypoint**: a way to run the autopilot daemon under
  the same policy without the console, so a crashed headless daemon is restarted
  under the same budget.
- A **liveness record** (lease/PID) for the autopilot daemon so a running daemon
  is discoverable and a restart adopts the live one instead of duplicating it or
  declaring it dead.
- Single-slot retirement in the worker pool: a slot that exhausts its budget is
  retired/quarantined while the remaining healthy members keep running; the
  budget becomes windowed rather than lifetime.

### Out of scope
- Any change to *why* a daemon crashes — the inner-loop containment, deadlines,
  and transient-error handling are phases P1–P3 and are assumed already shipped
  (see D-A). This phase only governs respawn of an already-crashing process.
- A daemon/process-manager dependency (systemd, supervisord, pm2). Supervision
  stays in-tree stdlib subprocess management, matching the existing supervisors.
- Auto-starting autopilot from a cold console (the deliberate manual-start
  default for unattended base-branch writes is unchanged; this phase governs
  *respawn of an already-running daemon*, not first launch).
- Changing the SIGTERM/SIGKILL group-shutdown semantics, lease/claim protocol,
  landing rules, or the agent execution path.
- A new persistent store table or schema column beyond what is needed for the
  autopilot liveness record (reuse existing store surfaces where possible).

### Must not regress
- Orphan-free group shutdown: `stop()` on any supervisor/pool still leaves no
  surviving member, agent, or MCP child (commit 36a0622 guarantee).
- Exactly-once execution and serialized landing under the worker pool.
- The worker never commits to the operator's branch except via the
  SubmitStrategy; respawn introduces no new write path to the base branch.
- `flywheel_core.task` / `flywheel_core.lifecycle` purity; the SDK stays optional
  behind `_sdk`; agent claims stay untrusted (a respawned daemon's self-report is
  never authoritative liveness).
- Single-worker (`concurrency = 1`) and `--once` drain behavior.

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its grader
type, visibility, the exact check, and the gaming move it forecloses.

1. When a supervised daemon child dies once inside its crash-loop window, the
   supervisor shall respawn it without any operator command, returning to a live
   supervised state. [command | held-out]
   verify: `uv run pytest packages/flywheel/tests/test_process_supervision.py -k respawn_within_window` — a test that spawns a child via the supervisor, kills it, advances the supervisor one tick, and asserts a new live child (a different pid) is owned with no `start()` call in between. Fails against today's code (status flips to DEAD and stays).
   defends against: faking respawn by relabeling the DEAD status to "supervised" without actually launching a new process — the check asserts a new live pid, not a string.

2. When a supervised daemon dies more than N times within the rolling window
   (N from the shared policy), the supervisor shall stop respawning and report a
   distinct DEAD-after-budget state queryable from outside the daemon. [command | held-out]
   verify: `uv run pytest packages/flywheel/tests/test_process_supervision.py -k budget_exhausted` — drive N+1 deaths inside one window with a child that exits non-zero immediately; assert the supervisor stops spawning new children after the (N+1)th and its status is the budget-exhausted terminal state (distinct from a single DEAD). Counts actual spawn calls, not log text.
   defends against: an "unbounded respawn that never reports exhaustion" passing by simply never stopping — the check asserts spawning *halts* and a distinct terminal status is reported.

3. While the crash-loop window has elapsed since the earliest counted death, the
   supervisor shall treat the budget as replenished, so a death after a long
   healthy interval is respawned rather than counted against an old burst.
   [command | held-out]
   verify: `uv run pytest packages/flywheel/tests/test_process_supervision.py -k window_decays` — record deaths up to the budget, advance the supervisor's clock past the window, induce one more death, assert it is respawned (budget treated as reset). Time is injected, not wall-clock.
   defends against: implementing a cheap *lifetime* counter (today's `MAX_POOL_RESTARTS_PER_SLOT`) that passes #2 but never decays — this case fails a lifetime counter and only passes a windowed one.

4. When a worker-pool slot exhausts its crash-loop budget, the pool shall retire
   that single slot and keep every other live member running, rather than
   group-killing the whole pool. [command | held-out]
   verify: `uv run pytest packages/flywheel-worktree/tests/test_worker_pool_safety.py -k slot_retire_keeps_fleet` — run a pool of size ≥ 2, force one slot to crash past budget while another slot stays healthy, assert the healthy member's pid is still alive after the failing slot retires (and the pool keeps supervising). Fails today (`finally: stop()` group-kills all).
   defends against: passing by lowering the budget so the test "happens not to" group-kill — the check asserts the surviving member is *still alive after* the failing slot is retired, under an explicit induced crash-past-budget.

5. When a headless autopilot daemon (started outside the console) dies once
   inside its window, the headless supervised entrypoint shall restart it under
   the same shared policy. [command | held-out]
   verify: `uv run pytest packages/flywheel-orchestrator/tests/test_autopilot_supervised_run.py -k headless_respawn` — invoke the headless supervised entrypoint with a child that dies once then succeeds; assert the daemon is restarted and continues (a second cycle runs), using injected spawn + clock. Fails today (no supervisor object for the headless path).
   defends against: satisfying "headless supervision exists" with a no-op flag that never restarts — the check asserts an observed restart and continued operation, not the presence of an entrypoint.

6. While an autopilot daemon is running, it shall publish a liveness record
   (PID + freshness) such that a second supervisor reading that record finds the
   live daemon and adopts it instead of spawning a duplicate or declaring it
   dead. [command | held-out]
   verify: `uv run pytest packages/flywheel-orchestrator/tests/test_autopilot_supervised_run.py -k liveness_adoption` — write/refresh a liveness record from a live daemon, point a second supervisor at it, assert the second supervisor reports the daemon discoverable/adopted (a DETACHED-or-adopted state) and spawns no second child. Asserts spawn count == 0.
   defends against: a stale record reading as "live" forever (so a truly dead daemon is never respawned) — the check includes a stale-record case that must read as not-live, mirroring the worker lease-expiry semantics.

7. If the shared supervision policy's budget is configured to 0 (or supervision
   is explicitly disabled), then the supervisor shall not respawn and shall
   report the death exactly as the pre-respawn behavior did, so the safety
   override is honored. [command | visible]
   verify: `uv run pytest packages/flywheel/tests/test_process_supervision.py -k budget_zero_no_respawn` — construct a supervisor with budget 0, kill its child, assert zero respawns and a plain DEAD status. 
   defends against: hardcoding respawn-always so the unattended-base-branch operator cannot turn it off — the check proves the override path produces no spawn.

8. The existing supervisor, pool, and autopilot-daemon suites shall still pass
   after the change. [command | held-out] (verification-surface)
   verify: `scripts/check.sh` runs green (ruff -> pyright -> pytest across the workspace), and specifically `uv run pytest packages/flywheel/tests/test_worker_supervisor.py packages/flywheel/tests/test_autopilot_supervisor.py packages/flywheel/tests/test_supervisor_group_shutdown.py packages/flywheel-worktree/tests/test_worker_pool.py packages/flywheel-worktree/tests/test_worker_pool_safety.py packages/flywheel-orchestrator/tests/test_autopilot_daemon.py` passes.
   defends against: making the new behavior pass by weakening or deleting the existing orphan-shutdown / exactly-once / drain assertions — those suites must stay green unmodified.

Verification surface: this feature touches the worker pool's restart/retirement
logic and adds new supervisor behavior that existing tests pin, so the gate
fired. The existing supervisor, pool, and autopilot-daemon suites still pass
after the change (criterion #8, held-out); any check relaxed or removed must be
named with an equal-or-greater replacement (a removed orphan-shutdown or
exactly-once assertion with no named replacement is a blocking defect); the new
respawn/budget/liveness behavior is proven by held-out command graders the
implementing agent did not author against its own known inputs (criteria
#1–#6), each driven by injected spawn/clock so the budget and window are
exercised deterministically rather than against wall-clock the agent controls.

## Decomposition Hint (for /fw-plan)
Splits along the shared invariant and its three consumer sites. The **shared
supervision policy** — a single object/value carrying `(respawn-on-death,
windowed crash-loop budget N-per-window, liveness-record read/write)` — is the
invariant every other slice asserts against; build and grade it first, then have
each supervisor adopt it so no slice inherits a red suite.

- Layer **policy core** (the shared invariant): a windowed crash-loop budget
  abstraction (count deaths in a rolling window, decide respawn-vs-exhausted,
  injected clock) plus a budget-0/disabled override. Satisfies #3, #7 directly
  and is the dependency of every site below. Grade it as observable
  decide-respawn/decide-exhausted behavior, not "the class exists."
- Layer **autopilot + worker supervisor adoption**: both console supervisors
  consume the policy so a death inside budget respawns and a death past budget
  reports DEAD-after-budget. Satisfies #1, #2; depends on policy core.
- Layer **worker-pool slot retirement**: replace the lifetime per-slot counter
  and the group-kill-on-exhaustion with windowed budget + single-slot
  retirement that spares healthy members. Satisfies #4; depends on policy core.
- Layer **autopilot liveness record + headless supervised entrypoint**: the
  daemon publishes a PID/freshness record (mirroring the worker `task_claims`
  lease-expiry semantics), and a headless supervised entrypoint runs the daemon
  under the same policy with adoption. Satisfies #5, #6; depends on policy core.

Shared invariants multiple layers assert against, name them so dependents update
together: the policy's budget/window parameter shape; the new DEAD-after-budget
status member added to `WorkerState`/`AutopilotState` (every status reader —
dashboard, TUI, tests — must handle the new member in the same change); the
liveness-record format and its staleness rule (must match the existing worker
lease-expiry semantics so #6's stale case behaves like a lapsed lease).

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-A: Supervision is the OUTERMOST loop; it is only safe atop P1–P3  (Status: Accepted)
- Context: Auto-respawning a daemon that crash-loops would be worse than leaving
  it dead — for autopilot it would mean an unbounded stream of unattended writes
  to the operator's base branch. | Decision: This phase (P6) ships *after* P1
  containment, P2 deadlines, and P3 transient resilience, which stop the inner
  loops from crash-looping; the bounded crash-loop budget is the interlock that
  makes respawn safe even if an inner loop regresses. Treat P1–P3 as assumed
  prerequisites; do not re-implement inner-loop containment here.
- Rejected: shipping supervision first (would crash-loop into the base branch);
  unbounded respawn (no interlock). | Consequences: respawn correctness depends
  on the budget being genuinely windowed and the exhausted state being terminal
  and loud — hence criteria #2, #3, #7 are held-out and non-negotiable.

### D-B: Auto-respawn under a windowed crash-loop budget — not notify-only, not unbounded  (Status: Accepted)
- Context: Today supervisors observe death and report DEAD with no restart; the
  dashboard tells the operator to respawn by hand. Notify-only fails the north
  star ("autopilot never stops"); unbounded respawn is unsafe (D-A). | Decision:
  The supervisor AUTO-respawns a dead daemon up to N times per rolling window;
  exceeding the budget surfaces a loud, queryable DEAD-after-budget state and
  stops respawning. One shared policy drives autopilot, worker, and pool.
- Rejected: notify-only ("type /autopilot start", today's behavior) — leaves the
  loop stopped; unbounded respawn — crash-loops; lifetime (non-decaying) budget
  (today's `MAX_POOL_RESTARTS_PER_SLOT`) — a long-healthy daemon that finally
  dips once is permanently un-respawnable. | Consequences: a windowed counter and
  injected clock are required (criterion #3); a configurable budget-0 override
  preserves the operator's ability to opt out for unattended base-branch safety
  (criterion #7).

### D-C: One slot's exhaustion retires that slot, never the fleet  (Status: Accepted)
- Context: `WorkerPool._supervise_tick` sets `_stop_requested` on budget
  exhaustion and the `finally: stop()` group-kills *every* live member — one bad
  slot kills healthy workers mid-task. | Decision: an exhausted slot is retired/
  quarantined; the pool keeps supervising and the other members keep running.
- Rejected: keep group-kill-on-exhaustion (collateral fleet death); ignore the
  budget and respawn forever (crash-loop, violates D-A). | Consequences: the
  pool's "return to size N" invariant is relaxed for retired slots — the pool
  may run below N when a slot is quarantined; that is the intended trade (a
  smaller live fleet beats a dead one).

### D-D: Autopilot gets a liveness record mirroring the worker lease  (Status: Accepted)
- Context: the worker supervisor detects liveness via the `task_claims`
  lease-expiry read and thus distinguishes DETACHED from DEAD; autopilot writes
  no record, so a detached daemon is unadoptable and a restart duplicates or
  mis-declares it. | Decision: the autopilot daemon publishes a PID/freshness
  liveness record with the *same* staleness semantics as the worker lease (a past
  freshness = not live), so a second supervisor adopts a live daemon and respawns
  a truly dead one. | Rejected: a bare always-live pidfile (a stale record would
  read live forever and block respawn — criterion #6's stale case forbids it);
  trusting the daemon's own self-reported "I'm alive" (agent/daemon claims are
  untrusted — liveness is read out-of-band from the record's freshness, not from
  a status the daemon asserts). | Consequences: introduces one liveness-record
  surface; staleness rule must match the lease semantics exactly so the two
  supervisors behave identically.

## Open Questions (accepted gaps)
None. Every criterion lowers to a `command` grader driven by injected
spawn/clock against this repo's pytest surface.

## Next Steps
Run `/fw-plan 00070-FEATURE-process-supervision` to compile these criteria into
flywheel tasks and graders.
