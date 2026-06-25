# 00051 — Held-out oracle registration: activate the gate, close the loop

Status: spec. Completes the recursion the WorkGraph/verification program has been
building toward. 00050 shipped an orchestrator-owned execute-time held-out gate
(`evaluate_held_out_gate`, `_held_out_gate.py`) and plumbed a `held_out_source`
through `run_worker`/`orchestrate` — but **the product CLI never constructs one**
(`flywheel_worktree.worker.main` calls `run_once(...)` with no `held_out_source`;
nothing under `packages/flywheel/src/` or `WorkPolicy` builds a source). So the
gate ships **dormant**: reachable only by direct test calls. Meanwhile `fw-verify`
produces discrimination-proven blind oracles under git-ignored
`.flywheel/verification/`, but its template explicitly forbids wiring them as
graders — a prohibition aimed at the *in-repo / in-worktree* path that is exactly
what 00050's out-of-worktree operator-owned source now supersedes. This spec wires
the two halves together: activate the gate from committed config, define the
durable shape that turns an admitted oracle into a held-out registration the gate
runs, and teach `fw-verify` to emit it — so a blind oracle proven to discriminate
at authoring time now **gates the agent's real run** at execute time.

## Outcome

An operator who has run `fw-verify` can make its admitted blind oracle gate the
real run with **no code change** — only committed config and a git-ignored
registration. Concretely: a `[held_out] root` pointer in `flywheel.toml` (committed)
points the orchestrator at a directory (git-ignored, e.g. under
`.flywheel/verification/`); when set, the shipped `flywheel worker` builds a
`FilesystemHeldOutGraderSource` at that root and runs the execute-time gate (00050)
for every task. `fw-verify`, after admitting an oracle, writes a registration at
`<root>/<task_id>.json` — a held-out **command** grader that invokes the admitted
oracle (by its absolute operator path) against the agent's committed tree. The
discrimination `fw-verify` proved blind now holds through the gate: a committed
result with correct behavior **lands**; a committed result that fails the oracle is
**blocked, parked, and recorded** (the 00050 fail path). When `[held_out] root` is
absent, behavior is **byte-identical to today** — the gate does not run, and neither
the registration root nor any oracle is present in any agent worktree.

## Background

The program's north star is verified build-conformance: the agent never decides
whether it succeeded; an out-of-band held-out check does. 00050 built that check's
engine and proved (its own `fw-verify`) that it grades the committed tree from
exit codes, fail-closed, never from the agent's self-report. But two gaps keep the
promise unkept end-to-end:

1. **The gate is unreachable in production.** `run_worker`/`orchestrate` accept
   `held_out_source`, but the `flywheel worker` entry point
   (`worker.py:main` -> `run_once`) never passes one, and no policy field or
   product-shell code constructs a `FilesystemHeldOutGraderSource`. Every shipped
   run today is ungated regardless of what an operator registers on disk.
2. **`fw-verify`'s oracles never reach the gate.** They are authored to a
   git-ignored scratch dir and the template's anti-patterns forbid landing them as
   graders — correctly, for the old in-repo path (a committed oracle sits in the
   agent's worktree and is gameable). 00050 introduced a *different* channel: an
   operator-owned source the orchestrator reads out-of-band, absent from the
   worktree. The recursion is to route admitted oracles through *that* channel.

The tacit, load-bearing requirement a literal agent will miss: **the value is the
committed pointer with the git-ignored payload.** A fresh git worktree never
materializes git-ignored files, so a `[held_out] root` pointing inside
`.flywheel/verification/` activates the gate for the orchestrator while the
registrations and oracles stay out of every agent worktree. Committing the *payload*
(registration or oracle) into the tree re-opens the exact gameability 00050 closed;
committing only the *pointer* does not. The two must not be conflated, and the
`fw-verify` fence against committing oracles into the repo must survive this change
intact — registration is a write to the out-of-worktree, git-ignored root, never a
commit into the tracked tree.

## Scope

### In scope
- **Gate activation from config.** A `[held_out] root` key in `flywheel.toml`,
  parsed into `WorkPolicy` (default unset). When set, the `flywheel worker` entry
  point constructs a `FilesystemHeldOutGraderSource` at that root (resolved
  relative to the repo root) and passes it through to `run_once`/`orchestrate`,
  activating the 00050 gate. When unset, no source is constructed and the gate
  never runs.
- **The registration shape.** A defined, validated way to express an admitted
  `fw-verify` oracle as a `<root>/<task_id>.json` held-out **command** grader that
  (a) `FilesystemHeldOutGraderSource.graders_for(task_id)` loads without error, and
  (b) invokes the oracle against the committed tree such that the gate's PASS/FAIL
  verdict reproduces the oracle's discrimination (PASS on a correct committed tree,
  FAIL on a tree that fails the oracle).
- **`fw-verify` emits the registration.** The `fw-verify` skill template, after
  admitting and recording an oracle, writes (or presents for the operator to apply)
  a `<root>/<task_id>.json` registration at the configured held-out root, keyed by
  task id, referencing the oracle by absolute operator path — while preserving the
  existing fence that the oracle is never committed into the tracked repo and never
  wired as an in-repo task grader.

### Out of scope
- Any change to the 00050 gate engine itself (`evaluate_held_out_gate`,
  `GateVerdict`, fail-closed semantics, the before-submit-under-lease placement).
  This spec only constructs and feeds the existing source; it does not re-decide
  the gate's behavior.
- Held-out `transcript`/`rubric`/`manual` graders, or any held-out grader type
  beyond `command` (00050 D-5 stands).
- Auto-discovering oracles without an `fw-verify` admission, or generating oracles.
  Only an admitted, discrimination-proven oracle is registered.
- A new `orchestrator_events` ledger (still the separate deferred spec); gate
  outcomes use 00050's existing recording surface.
- Changing where `fw-verify` authors oracles (still git-ignored
  `.flywheel/verification/`) or its discrimination/flake gates.
- flywheel-core: `Task`, `Grader`, lifecycle, harness, schema — all unchanged.

### Must not regress
- With `[held_out] root` absent, the gate does not run and landing is
  byte-identical to today (the 00050 D-7 no-gate path, now also the config default).
- An existing `flywheel.toml` with no `[held_out]` table loads unchanged.
- When the gate is active, neither the registration root nor any registered oracle
  is present in the agent's worktree during its run.
- 00050's gate semantics (fail-closed, exit-code verdict, NO_GATE for an
  unregistered task, before-submit-under-lease) are unchanged.
- `fw-verify`'s existing fence — oracle authored to git-ignored scratch, never
  committed into the tracked repo, never wired as an in-repo task grader — still
  holds; the new registration writes only to the out-of-worktree held-out root.
- flywheel-core's lifecycle, `Task`/`Grader`, and schema are unchanged.

## Success Criteria

Each criterion grades an observable end-state — the landed/blocked result, the
loaded registration, the rendered template, or what is present in the worktree —
never a call sequence. Tests live under
`packages/flywheel-orchestrator/tests/` and `packages/flywheel-worktree/tests/`.

1. When `flywheel.toml` declares a `[held_out] root` and a task has a registration
   whose command grader passes against the committed tree, the task reaches the
   same landed end-state as a no-held-out baseline; when the registration's grader
   fails, the task does not land (no merge/PR effect, worktree parked).
   [command | held-out]
   verify: a worktree/orchestrator test loads a policy with `[held_out] root` set,
   drives one task whose registered grader passes and one whose registered grader
   fails, and asserts the pass-task lands identically to a no-held-out baseline
   while the fail-task is blocked and parked.
   defends against: shipping an inert config key — parsing the root but never
   constructing the source, so the gate stays dormant and every run lands ungated.

2. When `flywheel.toml` has no `[held_out]` table (or no `root`), the worker builds
   no held-out source and landing is byte-identical to today for every task,
   including a task that happens to have a `<task_id>.json` file on disk.
   [command | held-out] (must-not-regress)
   verify: the existing orchestrator + worktree suites pass unchanged; plus a test
   with no `[held_out]` config asserts no gate runs even when a registration file
   exists at a path the worker is not pointed at, and the task lands as today.
   defends against: a default that silently activates gating (or reads a
   conventional path) for operators who never opted in, changing landing behavior
   on upgrade.

3. The worker resolves `[held_out] root` relative to the repo root and constructs a
   `FilesystemHeldOutGraderSource` at exactly that directory; a relative root in
   `flywheel.toml` resolves to `<repo_root>/<root>`. [command | held-out]
   verify: a test sets a relative `[held_out] root`, runs the worker entry path,
   and asserts the gate consults registrations under `<repo_root>/<root>/` (a
   registration there is honored; an identically named file elsewhere is not).
   defends against: resolving the root against the worker's cwd or a sandbox path
   so registrations silently miss and tasks land ungated.

4. A registration produced for an admitted oracle by the defined shape is loadable
   by `FilesystemHeldOutGraderSource.graders_for(task_id)` without raising, and
   yields one or more `command` graders. [command | held-out]
   verify: a test builds a registration via the defined shape for a sample oracle,
   then asserts `graders_for(task_id)` returns command graders (no
   `HeldOutGraderError`), and that a malformed/non-command registration still
   fails closed (the 00050 contract).
   defends against: emitting a registration the gate cannot parse, which fails
   closed and blocks every run — or one that smuggles a non-command grader past
   00050's command-only rule.

5. When the registration's oracle command is run through the gate against a
   committed tree containing a CORRECT implementation it passes, and against a tree
   containing a plausible-WRONG implementation it fails — i.e. the oracle's
   authoring-time discrimination is reproduced through the execute-time gate.
   [command | held-out]
   verify: a test registers an oracle with a known discriminating input, runs the
   gate against a committed tree with a correct reference (asserts PASS / lands)
   and against a tree with a seeded-wrong reference (asserts FAIL / blocked),
   reproducing the kill-and-pass `fw-verify` recorded.
   defends against: a registration that runs the oracle but loses its
   discrimination (wrong cwd so the oracle imports nothing, a swallowed exit code,
   the oracle pointed at a stale tree) — a green-on-everything gate that grades
   nothing.

6. The registered oracle is evaluated with the agent's committed tree as its
   working directory, so the oracle observes the committed changes; the oracle file
   itself is referenced by a path outside that tree. [command | visible]
   verify: a test whose registered oracle asserts on a file the agent committed
   shows the gate verdict depends on the committed content, while the oracle file
   resides outside the committed tree (an absolute operator path).
   defends against: grading a stale or empty tree, or requiring the oracle file to
   be inside the worktree (which would re-land it into the agent's view).

7. When the gate is active, neither the held-out root nor any registered oracle is
   present anywhere under the agent's worktree during its run. [command | held-out]
   verify: a test with `[held_out] root` set and a registration + oracle on disk
   asserts that during the agent's run the worktree contains neither the
   registration file nor the oracle file (the committed-pointer / git-ignored-payload
   property).
   defends against: activation that copies or materializes the registration or
   oracle into the worktree, exposing the held-out check to the agent and making
   "held-out" a lie.

8. After rendering, the `fw-verify` skill instructs writing the admitted oracle's
   registration to the configured held-out root keyed by task id, AND still forbids
   committing the oracle into the tracked repo or wiring it as an in-repo task
   grader. [command | held-out]
   verify: a test renders the `fw-verify` skill (via the skills renderer) and
   asserts the rendered text (a) instructs registration at the held-out root keyed
   by task id and (b) retains the fence against committing the oracle into the
   repo / wiring it as an in-repo `command` grader; and that no `__FW_` token
   survives rendering.
   defends against: a template that either never closes the loop (no registration
   guidance) or drops the fence (telling the agent to commit the oracle), re-opening
   in-repo gameability.

9. The orchestrator, worktree, and core test suites still pass after the change.
   [command | held-out] (verification-surface)
   verify: `uv run pytest packages/flywheel-orchestrator/tests/`,
   `uv run pytest packages/flywheel-worktree/tests/`, and
   `uv run pytest packages/flywheel-core/tests/` all pass.
   defends against: satisfying a new criterion by weakening or deleting an existing
   gating/landing/policy test.

10. flywheel-core's lifecycle, `Task`/`Grader` definitions, and schema are unchanged
    by this feature. [command | held-out] (verification-surface)
    verify: `uv run pytest packages/flywheel-core/tests/` passes unchanged and
    `git diff` touches no file under
    `packages/flywheel-core/src/flywheel_core/`.
    defends against: pushing activation/registration concerns into core — held-out
    gating stays an orchestration concern.

Verification surface: this feature ACTIVATES and feeds a grading-and-landing gate —
it IS a verification surface. Definition of Done (inherited by every task, all
held-out where possible): the existing orchestrator + worktree + core suites still
pass (#9); no flywheel-core change (#10); the gate stays inert without config (#2);
activation never places the held-out payload in the worktree (#7); a registered
oracle reproduces its discrimination through the gate (#5). No grading assertion may
be relaxed, skipped, or deleted; a removed assertion with no equal-or-stronger
replacement is a blocking defect.

## Decomposition Hint (for /fw-plan)
- Layer **gate activation (config -> source -> worker)**: satisfies #1 (activation
  side), #2, #3, #7. Adds the `[held_out] root` policy field + validator and
  constructs `FilesystemHeldOutGraderSource` in the `flywheel worker` entry path,
  forwarding it to `run_once`. No dependency on the registration shape (the source
  type already exists from 00050).
- Layer **registration shape (oracle -> `<root>/<task_id>.json`)**: satisfies #4,
  #5, #6, and the landing side of #1. Defines and validates the held-out command
  grader that invokes an admitted oracle by absolute path against the committed
  tree, round-tripping through 00050's source and reproducing discrimination
  through the gate. Depends on the activation layer for the end-to-end land/block
  assertions.
- Layer **`fw-verify` template recursion**: satisfies #8. Updates the
  `_skill_templates/fw-verify.md` step/anti-patterns to emit the registration at
  the configured root while preserving the no-commit-into-repo fence; renders
  cleanly. Depends on the registration shape being defined (it instructs producing
  it).

Shared invariants multiple layers assert against:
- The held-out root location semantics (committed pointer in `flywheel.toml`,
  git-ignored payload, resolved relative to repo root, absent from the worktree) —
  defined by the activation layer; the registration and template layers reference
  it.
- The registration format (`<root>/<task_id>.json`, command grader, oracle by
  absolute path, `cwd = committed tree`) — defined by the registration layer,
  consumed by 00050's source and described by the template layer. It is the same
  on-disk contract `FilesystemHeldOutGraderSource` already reads
  (`_held_out_gate.py:126-184`).

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-1: Activation is a committed `[held_out] root` pointer with a git-ignored payload  (Status: Accepted)
- Context: the gate must be activatable by config alone, without exposing the
  held-out check to the agent. A fresh git worktree materializes only tracked
  files, so a committed pointer into a git-ignored directory activates the gate for
  the orchestrator while keeping registrations and oracles out of every worktree.
- Decision: `flywheel.toml` carries a committed `[held_out] root` key; the payload
  it points at (registrations + oracles, conventionally under
  `.flywheel/verification/`) stays git-ignored. The worker resolves the root
  relative to the repo root and builds `FilesystemHeldOutGraderSource` there.
- Rejected: committing the registrations/oracles themselves (they would land in the
  worktree — the in-repo gameability 00050 closed); a hidden field on `Task`
  (00050 D-2 rejected this); an env var only (not committed, so the gate's
  activation is not reproducible from the repo).
- Consequences: #3 pins repo-root resolution; #7 pins worktree-absence; #2 pins the
  absent-config default. The pointer is reproducible config; the payload is local.

### D-2: A registered oracle is a command grader invoking the oracle by absolute path, cwd = committed tree  (Status: Accepted)
- Context: the oracle file lives at the operator's git-ignored root, outside the
  agent's committed tree; the gate (00050) runs command graders with
  `cwd = committed_tree` and inherits the worker env
  (`run_command_graders`, grader_command.py:93-147).
- Decision: the registration's `run` invokes the oracle by its absolute operator
  path; the gate runs it with the committed tree as cwd, so the oracle imports/loads
  the agent's committed code while the oracle source stays outside the tree.
- Rejected: copying the oracle into the committed tree at gate time (mutates and
  re-lands the very tree being graded, re-exposing the oracle); a relative oracle
  path (resolves against the worktree under `cwd`, where the oracle does not exist —
  fails closed on every run).
- Consequences: #5 reproduces discrimination through the gate; #6 pins
  committed-tree-as-cwd with the oracle outside it. The registration is
  operator-machine-local (an absolute path), acceptable because it is git-ignored
  payload, not committed config.

### D-3: The gate is inert unless `[held_out] root` is configured  (Status: Accepted)
- Context: the overwhelming majority of repos/tasks have no held-out registration;
  activation must be opt-in and upgrade-safe.
- Decision: absent `[held_out] root`, the worker constructs no source and the gate
  never runs — byte-identical to today (00050 D-7 at the config layer).
- Rejected: defaulting the root to a conventional path (silently activates gating on
  upgrade for anyone with a stray file there); always-on with an empty source
  (imposes a skip-path and risks the default landing flow).
- Consequences: #2 is the must-not-regress anchor; an existing `flywheel.toml`
  loads and behaves unchanged.

### D-4: The `fw-verify` no-commit fence survives; registration is the sanctioned out-of-worktree channel  (Status: Accepted)
- Context: the `fw-verify` template forbids landing oracles as graders. That
  prohibition targets the in-repo / in-worktree path (a committed oracle is in the
  agent's view, gameable). 00050 introduced an out-of-worktree operator-owned
  channel the template predates.
- Decision: the template gains a step to write the admitted oracle's registration to
  the configured held-out root (out-of-worktree, git-ignored) keyed by task id,
  while keeping every existing anti-pattern against committing the oracle into the
  tracked repo or wiring it as an in-repo `command` grader. The two are distinct:
  registration writes to the git-ignored root; the forbidden move commits into the
  tree.
- Rejected: removing the no-commit fence (re-opens in-repo gameability); leaving the
  template unchanged (the loop never closes — admitted oracles never gate a run).
- Consequences: #8 grades both halves (instructs registration AND retains the fence)
  on the rendered template.

### D-5: This spec does not alter the 00050 gate engine  (Status: Accepted)
- Context: 00050's `evaluate_held_out_gate` already loads from a
  `HeldOutGraderSource`, fails closed, derives the verdict from exit codes, and runs
  before submit under the lease. This spec supplies and activates a source; it does
  not re-decide gate behavior.
- Decision: reuse `FilesystemHeldOutGraderSource` and the 00050 gate unchanged; add
  only construction/activation, the registration shape, and the template step.
- Rejected: re-implementing a parallel gate or source (duplicates 00050, risks
  divergent semantics).
- Consequences: #4 asserts the registration round-trips through the *existing*
  source; the gate's fail-closed / exit-code / before-submit semantics are inherited,
  not respecified.

## Open Questions (accepted gaps)
None. Every criterion lowers to a `command` grader over the orchestrator and
worktree suites (and the skills renderer for #8). The exact policy-field name, the
registration writer's location, and whether the template uses a new
`__FW_HELD_OUT_DIR__` placeholder or reads `[held_out] root` descriptively are
implementation choices for /fw-plan — the spec grades the activation, registration,
and recursion behavior, not the path.

## Next Steps
Run `/fw-plan 00051-held-out-oracle-registration` to compile these criteria into
flywheel tasks and graders.
