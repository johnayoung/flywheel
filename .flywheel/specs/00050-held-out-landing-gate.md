# 00050 — Execute-time held-out landing gate

Status: spec. Closes the gap `fw-verify` has now flagged twice (the WorkGraph
foundation and 00049): the held-out promise is only authoritative at *authoring*
time. This adds the execute-time half — an orchestrator-owned gate that grades an
agent's committed result with **operator-declared graders the agent never saw**,
run out of the agent's worktree at landing time, and blocks the land when they
fail. flywheel-core is untouched; held-out gating is purely an orchestration
concern.

## Outcome

An operator can register **held-out graders** for a task in a location the
orchestrator reads but the agent never receives — they are not part of the
`Task` handed to the harness and are absent from the agent's worktree during its
run. After the agent's run finalizes, while the task's lease is still held and
**before** `SubmitStrategy.submit`, the orchestrator runs those held-out graders
against the committed result in a subprocess it owns. If every held-out grader
passes, the task lands exactly as it does today (the gate is invisible on
success). If any fails — or cannot be run — the task does **not** land: no
merge/PR effect, the worktree is parked for forensics, and a distinct
"held-out gate failed" outcome is recorded, separable from both a clean land and
an agent-run failure. A task with no registered held-out graders is gated by
nothing and lands byte-identically to today.

## Background

Today all graders come from `task.graders` (task.py:100), which the agent
receives and can read; the only out-of-worktree re-grading that exists is
`GitWorktreeSubmitter._reverify` (worker.py:617-656), and it re-runs the *task's
own* command graders, only on the rebase path, recording nothing. There is no
operator-owned grader the agent cannot see (the `.flywheel/verification/`
held-out oracles `fw-verify` produces are git-ignored scratch, referenced only
in a planning comment at worker.py:797, and never gate a run). So every
`fw-verify` proof to date establishes that a discriminating oracle *exists* —
not that the agent's real run was graded by a test it never authored. The tacit,
load-bearing requirement a literal agent will miss: the value is entirely in the
agent *not being able to reach* the check. A held-out grader that leaks into the
worktree, the `Task`, or the agent's view is not held-out at all; and a gate that
silently passes when its held-out grader cannot be found is worse than no gate,
because it certifies ungated work as gated. The gate must therefore be hidden,
out-of-band, authoritative, and fail-closed.

The natural seam is the orchestrator's per-task drive (`_drive_under_lease`,
_orchestrate.py:1080-1252), which already holds the committed terminal status,
the sandbox path, the task, and the run id at the point it calls
`SubmitStrategy.submit` under the lease (_orchestrate.py:1193). Owning the gate
there keeps it strategy-agnostic (merge, pr, future container) and matches the
"out-of-worktree grading owned by the orchestrator" posture.

## Scope

### In scope
- A held-out grader source: operator-declared **command** graders the
  orchestrator loads from a location keyed by task id, never written into the
  agent's worktree and never part of the `Task` handed to the harness/agent.
- The gate: an orchestrator step in the per-task drive that, after the run
  finalizes and before `submit`, runs a task's held-out graders (if any) against
  the committed result in a subprocess the orchestrator owns, and computes a
  pass/fail verdict from their exit codes.
- Landing integration: on all-pass, land unchanged; on any fail, block the land
  (no `submit` landing effect), park the worktree, and record a distinct
  gate-failed outcome.
- Fail-closed behavior: a registered held-out grader that cannot be located or
  executed blocks the land and is recorded failed — never silently passed.

### Out of scope
- Held-out `transcript` / `rubric` / `manual` graders. The gate runs command
  graders against the committed tree only (mirrors `_reverify`; the other types
  judge the agent's process or work, not the committed result).
- A new `orchestrator_events` ledger for gate outcomes (that is the deferred
  events-ledger spec). Recording uses the orchestrator's existing run-record /
  work-report surface.
- Any change to flywheel-core: `Task`, `Grader`, the lifecycle, the harness
  verification loop, or core's schema. The gate never re-judges the agent's
  attempt status; it is a landing decision.
- Automatically promoting `fw-verify`'s git-ignored `.flywheel/verification/`
  oracles into held-out graders. The gate consumes operator-declared held-out
  graders; wiring fw-verify output into them is a later convenience.
- Container/remote sandbox specifics; the gate grades the committed working tree
  whatever the sandbox produced it.

### Must not regress
- A task with no registered held-out graders lands exactly as today; the gate
  does not run for it.
- In-run `task.graders` execution (harness.py:_validate) is unchanged.
- Existing `merge` / `pr` submit behavior for ungated tasks is unchanged,
  including the existing rebase `_reverify`.
- flywheel-core's lifecycle, `Task`/`Grader`, and schema are unchanged.

## Success Criteria

Each criterion grades an observable end-state — the landed/blocked result, the
recorded outcome, or what the agent could reach — never a call sequence. Tests
live under `packages/flywheel-orchestrator/tests/` (and
`packages/flywheel-worktree/tests/` where the landing effect is observed).

1. When a task has registered held-out graders that all pass against the
   committed result, the task reaches the same landed end-state as an otherwise
   identical task with no held-out graders. [command | held-out]
   verify: an orchestrator test drives a task whose held-out grader passes and
   asserts the same landing effect (submit invoked / merge landed) as a
   no-held-out baseline with the same terminal status.
   defends against: a gate that perturbs the success path — blocking, delaying,
   or altering the status of a task that actually passed.

2. When at least one held-out grader fails against the committed result, the task
   does not land: `SubmitStrategy.submit` produces no landing effect (no
   merge/PR) and the agent's worktree is retained. [command | held-out]
   verify: an orchestrator/worktree test injects a held-out grader that exits
   non-zero and asserts no merge/PR landing occurred and the branch/worktree is
   parked.
   defends against: the headline failure — letting work that failed the held-out
   check land anyway.

3. During the agent's run, the held-out grader is not part of the `Task` the
   harness receives and its content is absent from the agent's worktree; the gate
   still executes it after the run. [command | held-out]
   verify: a test asserts (a) the `Task` passed to the harness exposes no
   held-out grader, (b) the held-out grader file is not present anywhere under
   the agent's worktree during the run, and (c) the gate nonetheless runs it
   against the committed tree afterward.
   defends against: leaking the held-out check into the agent's view (its task,
   its context, or its worktree) where a capable agent could read it and satisfy
   the literal check — making "held-out" a lie.

4. When the agent's run reports a passing terminal status but a held-out grader
   exits non-zero, the gate blocks the land. [command | held-out]
   verify: a test sets the agent outcome to a DONE/passing status while a
   held-out grader fails, and asserts the land is blocked (the verdict came from
   the grader, not the agent's report).
   defends against: trusting the agent's self-reported status to pass the gate —
   the gate's verdict must be the out-of-band grader exit code.

5. If a registered held-out grader cannot be located or executed, the gate blocks
   the land and records the gate as failed — it never treats an unrunnable
   held-out grader as a pass. [command | held-out]
   verify: a test registers a held-out grader that is missing / errors on launch
   and asserts the land is blocked and recorded failed (fail-closed), not landed.
   defends against: the cheapest reward-hack — arranging for the held-out check
   to be unrunnable (deleted, renamed, erroring) so it is skipped and the work
   lands ungated.

6. A held-out gate failure is recorded as an outcome distinguishable from both a
   clean land and an agent-run failure. [command | held-out]
   verify: a test reads back the orchestrator's recorded result for three runs —
   landed-ok, agent-failed, gate-failed — and asserts the gate-failed case
   carries a marker distinct from the other two.
   defends against: a gate failure that reads as a clean DONE-land (invisible) or
   is conflated with an agent failure (misattributed), so an operator cannot tell
   the held-out gate caught something.

7. When a task has no registered held-out graders, the gate does not run and the
   landing behavior is byte-identical to today's. [command | held-out] (must-not-regress)
   verify: the existing orchestrator and worktree submit suites pass unchanged,
   plus a test asserting a no-held-out task lands exactly as before.
   defends against: imposing the gate, its overhead, or a spurious skip-path on
   tasks that never opted in.

8. The gate runs while the task's lease is held and before `SubmitStrategy.submit`
   is invoked, so a gate-blocked task is never submitted. [command | held-out]
   verify: a test asserts that for a blocked task the `submit` hook is not invoked
   with a landing status, and that the gate executes within the lease window.
   defends against: gating after landing (too late to block) or outside the lease
   (a second worker could land the same task concurrently).

9. A held-out command grader is evaluated against the committed result as its
   working tree, observing the agent's committed changes. [command | visible]
   verify: a test whose held-out grader inspects a file the agent committed
   asserts the grader sees the committed content (passes/fails based on it).
   defends against: grading a stale, empty, or pre-run tree instead of the
   agent's actual committed result.

10. The orchestrator, worktree, and core test suites still pass after the change.
    [command | held-out] (verification-surface)
    verify: `uv run pytest packages/flywheel-orchestrator/tests/`,
    `uv run pytest packages/flywheel-worktree/tests/`, and
    `uv run pytest packages/flywheel-core/tests/` all pass.
    defends against: satisfying a new criterion by weakening or deleting an
    existing landing/grading test.

11. flywheel-core's lifecycle, `Task`/`Grader` definitions, and schema are
    unchanged by this feature. [command | held-out] (verification-surface)
    verify: `uv run pytest packages/flywheel-core/tests/` passes unchanged and
    `git diff` touches no file under `packages/flywheel-core/src/flywheel_core/`.
    defends against: pushing held-out-gate concerns into core (a hidden grader
    field on `Task`, a lifecycle transition) — held-out gating is an
    orchestration concern and core stays agnostic to who lands its results.

Verification surface: this feature ADDS a grading-and-landing gate — it IS a
verification surface. Definition of Done (inherited by every task, all held-out
where possible): the existing orchestrator + worktree + core suites still pass
(criterion #10); no flywheel-core change (#11); ungated tasks land unchanged (#7);
the gate's authoritative verdict is the out-of-band grader exit code, never an
agent report (#4); the gate fails closed (#5). No grading assertion may be
relaxed, skipped, or deleted; a removed assertion with no equal-or-stronger
replacement is a blocking defect.

## Decomposition Hint (for /fw-plan)
- Layer **held-out grader source + isolation**: satisfies #3 (and the source
  side of #9). Defines where held-out graders live (operator-owned, keyed by task
  id, read by the orchestrator), guarantees they are absent from the agent's
  `Task` and worktree. No landing dependency.
- Layer **gate execution + verdict (fail-closed)**: satisfies #4, #5, #9. Runs
  the held-out command graders against the committed result in an
  orchestrator-owned subprocess and computes the verdict from exit codes, failing
  closed when a grader cannot run. Depends on the source layer.
- Layer **landing integration + recorded outcome**: satisfies #1, #2, #6, #7, #8.
  Wires the verdict into the per-task drive before `submit`: pass -> land
  unchanged, fail -> block + park + record a distinct outcome, none-registered ->
  no gate. Depends on the verdict layer.

Shared invariants multiple layers assert against:
- The held-out grader source contract (location + that it is excluded from the
  agent-facing `Task` and worktree) — defined by the source layer; the gate and
  the integration both read it.
- The gate-verdict type (pass / fail-with-reason / unrunnable-fails-closed) —
  produced by the execution layer, consumed by the integration layer.
- The point in `_drive_under_lease` where the gate sits relative to `submit`
  (before it, under the lease) — the integration layer pins it; #8 asserts it.

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-1: The gate is orchestrator-owned, in the per-task drive before submit  (Status: Accepted)
- Context: WHERE out-of-worktree held-out grading attaches. `_drive_under_lease`
  already holds the committed status, sandbox, task, and run id at the
  `submit` call under the lease (_orchestrate.py:1193); `_reverify` shows
  out-of-worktree command grading is feasible there.
- Decision: the orchestrator runs the gate in the per-task drive, after the run
  finalizes and before `SubmitStrategy.submit`. Strategy-agnostic by
  construction (merge, pr, future container all flow through the same drive).
- Rejected: inside `GitWorktreeSubmitter` (couples the gate to the git strategy;
  pr/container would not inherit it); inside flywheel-core (violates the
  workspace hard line — core is task-agnostic and git-free).
- Consequences: the gate composes with any `SubmitStrategy`; criterion #8 pins
  the before-submit-under-lease timing.

### D-2: Held-out graders are operator-owned, keyed by task id, excluded from the agent's Task and worktree  (Status: Accepted)
- Context: the entire value is the agent not being able to reach the check.
- Decision: held-out graders live in a location the orchestrator reads, keyed by
  task id; they are never merged into the `Task` handed to the harness and never
  written into the agent's worktree.
- Rejected: a hidden field on `Task` stripped before the agent (fragile — one
  serialization leak exposes it; muddies the input-agnostic `Task` contract);
  inline in `flywheel.toml` (cannot express per-task held-out checks and lives in
  a file the worktree contains).
- Consequences: criterion #3 grades the isolation directly (not in `Task`, not in
  worktree, still executed). The exact on-disk format is an implementation choice
  for /fw-plan; the spec grades the behavior, not the path.

### D-3: The gate fails closed  (Status: Accepted)
- Context: the cheapest reward-hack against any hidden check is to make it
  unrunnable so it is skipped.
- Decision: a registered held-out grader that cannot be located or executed
  blocks the land and is recorded failed.
- Rejected: fail-open (skip an unrunnable held-out grader) — turns the gate into
  a no-op exactly when it is being attacked.
- Consequences: criterion #5 is load-bearing; "no held-out registered" (#7) and
  "registered but unrunnable" (#5) are different paths — the former is today's
  behavior, the latter blocks.

### D-4: The verdict is the out-of-band grader exit code; agent self-report is never authoritative  (Status: Accepted)
- Context: the standing flywheel invariant — agent claims feed verification,
  never authoritative state.
- Decision: the gate's pass/fail is computed from the held-out grader's
  subprocess exit code against the committed tree, regardless of the agent's
  reported terminal status.
- Rejected: short-circuiting the gate on a DONE self-report — re-opens exactly
  the gameability the gate exists to close.
- Consequences: criterion #4 pins agent-says-pass + grader-says-fail -> blocked.

### D-5: Command graders only  (Status: Accepted)
- Context: the gate grades the committed tree out-of-band, like `_reverify`,
  which deliberately re-runs only command graders.
- Decision: held-out graders are command graders. Transcript/rubric/manual are
  out of scope at this gate.
- Rejected: held-out rubric/manual — they judge the agent's process or work, are
  not deterministic out-of-band tree checks, and an LLM judge is the flakiest
  signal to hang a landing decision on.
- Consequences: criterion #9 grades a command grader against the committed tree.

### D-6: Gate outcomes use the existing run-record/report surface, not a new ledger  (Status: Accepted)
- Context: a distinct gate-failed outcome must be recorded and visible, but the
  `orchestrator_events` ledger is a separate deferred spec.
- Decision: record the gate-failed outcome via the orchestrator's existing
  run-record / work-report surface, distinguishable from landed-ok and
  agent-failed.
- Rejected: requiring the events ledger now (couples this gate to deferred work);
  reusing the agent's lifecycle FAILED status (misattributes a landing decision
  as an agent-attempt failure).
- Consequences: criterion #6 grades the distinctness on the existing surface.

### D-7: The gate is active only when held-out graders are registered  (Status: Accepted)
- Context: the gate must not change behavior for the overwhelming majority of
  tasks that have no held-out graders.
- Decision: absent any registered held-out grader for a task, the gate does not
  run and landing is byte-identical to today.
- Rejected: an always-on gate (imposes overhead and a skip-path on opted-out
  tasks; risks regressing the default landing flow).
- Consequences: criterion #7 is the must-not-regress anchor.

## Open Questions (accepted gaps)
None. Every criterion lowers to a `command` grader over the orchestrator
(and worktree / core) test suites. The held-out grader on-disk format is an
implementation choice deferred to /fw-plan, not an un-gradeable criterion — the
spec grades the isolation and gating behavior, not the path.

## Next Steps
Run `/fw-plan 00050-held-out-landing-gate` to compile these criteria into
flywheel tasks and graders.
