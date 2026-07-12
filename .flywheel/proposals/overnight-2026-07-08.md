# Improvement proposals: overnight-2026-07-08

**Source:** operator run report, 18-hour overnight run of 2026-07-08/09 (delivered in conversation, 2026-07-09; no `/fw-retro` audit exists for this scope). Every pointer below was re-verified against main @ `847fe4f` before proposal. Sibling retros in `.flywheel/audits/` consulted for recurrence.
**Proposed:** 2026-07-09

## Summary

| Metric                  | N |
| ----------------------- | - |
| Findings reviewed       | 6 |
| Proposals (clustered)   | 4 |
| Advancing via /fw-spec  | 1 |
| Advancing via /fw-plan  | 3 |
| Accepted -- do not fix  | 0 |

Operator pre-selected "fix all" in the originating request; the STEP-7 selection question was therefore not re-asked.

## Proposals (ranked by leverage)

### P1 -- Operator control plane takes effect end-to-end under the postgres store

**Problem**
- "`_enqueue_control_command` (workflow.py:~1204) hardcodes SqliteStore, so fw approve/reject/say/interrupt all fail with 'unknown to this store'. Spec 00075 flipped reads but missed the command queue. Workaround: direct INSERT INTO control_commands in postgres." Re-verified: `packages/flywheel-core/src/flywheel_core/workflow.py:1181` opens `SqliteStore(db_path)` unconditionally while sibling paths in the same module (lines 799-833) already accept the injected backend.
- "Even a correctly-enqueued approve is never consumed: orchestrate() marks an awaiting-approval run in a session-lifetime attempted_approve set after one empty sweep (_orchestrate.py:2351) and never re-checks. Workaround: recycle one pool member so a fresh session sweeps." Re-verified: `_orchestrate.py:2351` (set created), `:2597` (skip), `:2610/:2625` (marked after one sweep).
- Clustered: two root causes, one capability -- fixing either alone still leaves `fw approve` inert.
- Recurrence: every approval-gated or steered run under the postgres store, which is now the default in both adopting repos (flywheel flipped 2026-07-08 with spec 00075; infrared same day). n=1 scope, but structurally recurs per gated run.

**Outcome**
- An `fw approve` (or reject/say/interrupt) issued while the store backend is postgres is accepted by the CLI and consumed by an already-running worker session -- no pool recycling, no manual SQL.

**Handoff:** `/fw-plan restore the end-to-end control plane under the postgres store: route the control-command queue through the same injected store backend spec 00075 gave the data plane, and make the orchestrator's empty-sweep approval mark expire instead of persisting for the session`
**Leverage:** recurs on every gated run under the now-default backend; blast radius is the entire operator control plane; cost-of-inaction is interrupt-driven hand SQL plus worker recycling per approval; fix-cost is small, in seams spec 00075 already carved.
**Operator decision:** advance (pre-selected in request)

### P2 -- Cross-phase prerequisites stay satisfied after the prerequisite's phase archives

**Problem**
- "Prereq satisfaction requires the prereq present in the active source listing; archival removes the file, so DONE-in-store prereqs read as 'missing' and dependents (reth-goldens, both *-lp-compose-goldens) stopped permanently. Workaround: copied the prereq JSONs from archive/ back to active/." Re-verified: `packages/flywheel-orchestrator/src/flywheel_orchestrator/_workflow.py:366-368` resolves prerequisites against a map built from the active listing only; the spec-00069 dangling-prereq re-driver then parks dependents with `prerequisite-missing` (`_claims.py:224`).
- Recurrence: n=3 tasks this run, single scope -- but structurally recurs on every phase archive that leaves cross-phase dependents behind, and the failure is silent-permanent (dependents park until a human notices).

**Outcome**
- A dependent whose prerequisite reached DONE in the store starts normally after the prerequisite's phase archives, with no file copied back from `archive/` to `active/`.

**Handoff:** `/fw-plan satisfy cross-phase prerequisites from store lifecycle (the authoritative state per docs/data-taxonomy) rather than requiring the prerequisite's presence in the active source listing`
**Leverage:** recurs every archive with cross-phase edges; blast radius is any dependent DAG tail; cost-of-inaction is permanent silent stalls; fix-cost moderate, confined to one resolution map, and taxonomy-aligned.
**Operator decision:** advance (pre-selected in request)

### P3 -- Attempt budgets for structurally-long work are operator-configurable

**Problem**
- Judge cap: "internal_error: 'Reached maximum number of turns (8)' on runs whose command graders all passed -- ~38% of early finishes died this way (~$17 of futile retries)" (rubric_judge_max_turns, flywheel-core harness.py:494).
- Iteration deadline: "Six different tasks structurally needed >30min/iteration and looped on deadline deaths (deadline_config.py:49) ... spec 00066's per-class toml override mapping exists in the file but nothing consumes it -- that's the real fix."
- Clustered: one underlying problem -- attempt budgets live as code constants, so any workload class that misfits them dies with no config recourse.
- Partially superseded: the default bumps are already on main (`8c95a47` turns 8->32, `847fe4f` 1800s->3600s). Residual re-verified: `rubric_judge_max_turns` appears only in `harness.py` and its test -- no toml/CLI surface; `deadline_config_from_mapping` (the spec-00066 `[deadlines]` TOML resolver) has zero consumers; `docs/configuration.md` documents neither.
- Recurrence: n=9 runs this scope (3 judge-cap deaths, 6 deadline loops); recurs whenever a new workload class misfits the constants.

**Outcome**
- An operator can set the rubric-judge turn budget and per-class deadlines in `flywheel.toml` and observe them take effect in a worker run, so the next long-workload class needs no editable-install patch.

**Handoff:** `/fw-plan make the rubric-judge turn budget and the per-class deadline overrides operator-configurable via flywheel.toml, consuming the existing spec-00066 resolver, and document the keys`
**Leverage:** largest observed waste this run (38% of early finishes + six looping tasks), but the default bumps already stopped the bleeding, so remaining cost-of-inaction is the next misfit class; fix-cost is cheap plumbing of an existing resolver.
**Operator decision:** advance (pre-selected in request)

### P4 -- Approval-parked and merge-bearing branches land without manual git surgery

**Problem**
- "Approval-parked runs never get their inline landing, and the landing re-driver's rebase strands merge-bearing branches (divergent-base, bit both proto tasks). Operator hand-landed via detached worktree + merge + full grader/verify ladder + git update-ref CAS." Re-verified: `packages/flywheel-worktree/src/flywheel_worktree/worker.py:441-469` (`_rebase_parked_branch`) rebases parked branches onto the phase base and aborts on conflict, which strands any branch carrying merge commits.
- Recurrence: n=2 tasks, single scope -- but the never-stop reliability program already flagged an "agentic rebase self-heal rung" as its open follow-up, so this is the second appearance of the rebase-recovery gap on record.

**Outcome**
- An approved run that was parked for landing reaches the standard landing ladder (submit-time verify included) without operator git surgery, including when its branch carries merge commits or a divergent base.

**Handoff:** `/fw-spec approval-parked and merge-bearing branches reach the standard landing ladder unattended -- the recovery mechanism for a branch the re-driver's rebase cannot fast-forward is the open design question`
**Leverage:** low observed recurrence but the highest per-event toil on record (manual grader ladder + ref CAS on the landing path, where an operator mistake ships unverified code); fix-cost is high and touches the verification gate -- discovery before change.
**Operator decision:** advance (pre-selected in request)

## Considered, not proposed (the auditable null result)

- Operator's local editable-install patches (judge turns 32, deadline 3600) -- superseded: identical defaults already committed on main as `8c95a47` and `847fe4f`; no action beyond P3's plumbing.
- ~$17 futile-retry spend and the three stuck tasks -- clustered into P3 (symptom of the judge-cap default, since fixed; the configurability residual is the proposal).
- `fw` console (TUI) twins of approve/say/interrupt -- clustered into P1: the session screen enqueues through a producer seam wired to the same store closure, so P1's outcome covers it; not a separate proposal.
- Hand-landing recipe preservation -- no proposal: the recipe lives in operator memory as a workaround record; P4's outcome makes it obsolete.

---

## Addendum (2026-07-09, post-cleanup forensics)

**Source:** operator branch-archaeology sweep after PR #233 merged in the adopting repo (infrared), plus the resulting RCA (delivered in conversation, 2026-07-09). Findings post-date the run report this file's P1-P4 were built from. Pointers re-verified against main @ `8afc9c0`. Summary: findings reviewed 5, proposals 3 (P5-P7); advancing via /fw-spec 1, /fw-plan 2.

### P5 -- Phase archival requires landed work and never erases fresh landing strands

**Problem**
- Silent lost landing, 2026-07-09 07:15 UTC: `balancer-bpt-base-golden` (infrared run `run-b11786796a0a4a6fbe62b7afb30f7bda`, $23.87, all graders + rubric green) parked `divergent-base` after a concurrent sibling advanced the base, exhausted the spec-00069 re-drive bound at 07:15:22.592 (`orchestrator_stop_events` id 14 -- the human-review queue entry), and **197ms later** (id 15) the archive sweep filed the phase and stamped the strand `stop-resolved`. The verified commit (`b45412e1`, the batch's first Base golden, spec-00036 criterion 2) is reachable from no base; no surface ever showed it. Found only by a manual `git cherry` sweep across surviving branches.
- Predicate defect: `archive_completed_phases` (`packages/flywheel-orchestrator/src/flywheel_orchestrator/_workflow.py:382`) archives on `all(_has_done_lifecycle(...))` alone -- landing state is never consulted. The stamping defect is design, not accident: "archival is the verified resolution act, so the stranded/stopped status surface clears with the phase" (`packages/flywheel-worktree/src/flywheel_worktree/worker.py:1228`, `archive_phases` docstring) -- a premise that is false exactly when the strand is fresh and terminal.
- Surface defect: `flywheel status` renders strands only for tasks present in the active listing, so archival alone (even without the stamp) removes a strand's last surface.
- Recurrence: n=1 silent loss this run -- but the identical strand on `proto-mcp-stdio-server` stayed visible ONLY because a sibling's approval park happened to block its phase archive. Strand visibility currently depends on an accident of phase composition; structurally recurs on every strand whose phase otherwise completes.
- Relation to spec 00076: 00076 makes stranded landings recoverable; P5 makes them impossible to hide while unrecovered. Either alone leaves the failure mode open (00076's bounded rungs can still exhaust into a park that today's archiver would erase; P5 without 00076 leaves loud-but-unrecovered parks).

**Outcome**
- A phase containing any DONE-but-unlanded run does not archive, and the sweep reports the blocking task. Landing-strand park kinds (`divergent-base`, `uncommitted-work`, `standing-verify`, `protected-paths`) are never stamped `stop-resolved` by archival -- only by a landability probe confirming the branch head is an ancestor of the base, or by an explicit operator resolution. A stranded run surfaces in `flywheel status` regardless of whether its task file is still in the active listing.

**Handoff:** `/fw-spec phase archival integrity: a phase archives only when every task is landed (not merely DONE), archival never services landing-strand stop events, and the stranded surface is store-backed so a strand outlives its phase's active listing`
**Leverage:** the only data-loss-class defect on record -- verified work invisible to every operator surface; cost-of-inaction is unbounded (silent losses are found only by manual branch archaeology); fix-cost small-to-moderate (one predicate, one stamping rule, one status source), sharing seams with 00076.
**Operator decision:** advance (operator-directed 2026-07-09)

### P6 -- Overlapping task surfaces imply a shared conflict key, enforced at validation

**Problem**
- The enabling collision behind both stranding incidents: `wsteth-goldens` carried `{infrared-internal-engine}` and `balancer-bpt-base-golden` `{golden-harness-base}` -- disjoint key sets, so they ran concurrently, yet both write the shared golden-harness surface (`tests/golden/harness.go`, `contracts.json`, chain cassettes). The loser's rebase conflicted (P5's trigger). `curve-balanced-compose-goldens` carried both keys and never collided -- the mechanism works when the planner declares it.
- `flywheel validate` already exists as the static task lint but checks graders only; key coverage is unchecked hope.
- Recurrence: any two tasks whose declared surfaces overlap under disjoint keys; goldens-style tasks (all writing shared harness/cassette files) are the standing example.

**Outcome**
- Task validation fails loudly, before dispatch, when two active tasks declare overlapping file surfaces without a shared conflict key.

**Handoff:** `/fw-plan extend flywheel validate (and the fw-plan authoring checklist) to derive per-task file surfaces from context.relevant plus grader paths and fail validation when two active tasks overlap without a shared conflict_key, with an explicit allow marker for reviewed-safe overlaps`
**Leverage:** removes the root enabler of both overnight stranding incidents; fix-cost small (a pairwise lint over data already in the task files); false-positive risk bounded by the allow marker.
**Operator decision:** advance (operator-directed 2026-07-09)

### P7 -- Run-loop resilience: session-limit awareness and ceiling-approach checkpoint nudges (clustered)

**Problem**
- Two account session-limit windows (resets 18:10 and 23:00 local) were classified as generic transient rate-limits: every queued task burned its full transient-retry budget against the wall and went retryable -- $0 spend, but two rounds of fleet-wide state churn, failure-history noise, and operator triage.
- Ceiling-adjacent loss pattern: agents treat a task as one atomic unit and die at the wall-clock ceiling with hours of uncommitted work (`curve-balanced-compose-goldens`: 6 runs and zero commits until an operator `say` ordering incremental checkpoint commits turned it around within 25 minutes; the same steer then carried `reth-goldens`). The remedy is proven (3-for-3) but was manual each time, via the P1-workaround SQL path.
- Clustered: both are the run loop failing to react to foreseeable, machine-detectable conditions (a parseable "resets X pm" refusal; a known ceiling with known remaining budget).

**Outcome**
- A session-limit refusal pauses claiming until the stated reset instead of burning per-task retry budgets; an agent approaching its iteration ceiling with no commit on its branch receives a harness-injected checkpoint instruction, so ceiling kills stop discarding progress.

**Handoff:** `/fw-plan session-limit-aware claim pausing (parse the reset time from the refusal and idle the pool until then, with a ceiling) and a ceiling-approach checkpoint nudge injected as an operator-style user turn at a configured remaining-time threshold when the branch has no new commits`
**Leverage:** medium -- toil/waste rather than loss (the churn self-heals; the nudge is a proven manual play); ranked below P5/P6 because neither loses work; fix-cost small-to-moderate.
**Operator decision:** advance (operator-directed 2026-07-09)

### Addendum: considered, not proposed

- Recovery of the lost commit itself (`b45412e1`) -- no flywheel proposal: it is an adopting-repo action (re-drive as a next-batch infrared task so graders re-prove the scenario against the current tree; branch + worktree preserved).
- `fw status --rollup` doc drift, `fw live` zero-token telemetry on streaming runs, `fw history` empty despite finished runs, cwd-sensitive source resolution ("no active tasks" from a subdirectory) -- observed repeatedly but diagnosis-grade only; no RCA performed this scope. Recorded here as retro seeds, not proposals.

## P8 (2026-07-11, infrared drain): conflict-resolution agent session crashes — asyncio.run() inside running loop

`_drive_conflict_resolution` (flywheel-worktree worker.py ~1466) is invoked via `asyncio.run()` from a context that already holds a running event loop; the session crashes with `RuntimeError: asyncio.run() cannot be called from a running event loop` and `RuntimeWarning: coroutine '_drive_conflict_resolution' was never awaited`. Every merge-conflict landing that escalates past FF/rebase/merge-fallback therefore parks instead of self-resolving — the designed recovery tier is dead code. Observed on `stableplain-lp-decompose-golden` (run parked after 3 re-attempts, reason merge-conflict; operator hand-landed: cassette JSONs deep-merged cleanly, all graders green). Fix: await the coroutine on the existing loop (or run in a fresh thread with its own loop).

RESOLVED 2026-07-12: both resolver drivers (default SDK and agents-runtime) now
run their session coroutine on a dedicated thread with its own event loop
(`_run_session_coroutine`), safe under orchestrate's running loop and in bare
synchronous callers. Regression-pinned in test_landing_agent_resolution.

## Weekend batch resolution (2026-07-12)

The retro seeds above plus the drain findings were verified and dispatched in
one batch: absorbed rebased copies now count landed by patch identity
(`git cherry`); prerequisites completed under a pre-cutover store are satisfied
by their archived task files; fw invoked from a subdirectory walks up to
flywheel.toml (and fails loud outside any repo); the archive sweep commits its
active->archive moves for tracked queues and names same-named-archive skips;
resolved strands stop rendering on active rows; rowless status lines carry a
`[no active row]` marker; worker/pool/heartbeat logs carry UTC timestamps; the
first-iteration heartbeat placeholder states that totals land at iteration
boundaries. `fw approve/say/interrupt` postgres routing and `fw live` counters
were verified already fixed on main (ad377d1, 2610b92) — the live-counter
observation is the iteration-boundary granularity, not a store split.
