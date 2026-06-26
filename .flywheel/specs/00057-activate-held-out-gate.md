# 00057 — Activate the held-out gate in this repo

Status: spec. Turns the verified-but-dormant execute-time held-out gate ON in
flywheel's own repo and locks the activation against regression. The machinery
shipped and was fw-verified in 00050 (gate engine) and 00051 (config activation +
registration shape + fw-verify emission): `load_effective_policy` parses
`[held_out] root` into `WorkPolicy.held_out_root`, the worker entry
(`build_held_out_source`, threaded through every `run_once`) builds a
`FilesystemHeldOutGraderSource` when the key is set, and `evaluate_held_out_gate`
runs it before submit. But flywheel's `flywheel.toml` carries no `[held_out] root`,
so the gate has stayed inert here — every WorkGraph spec since 00051 recorded
"registration deferred (no `[held_out] root` in flywheel.toml)". This spec commits
that pointer and proves, through the real worker construction path, that the
activated gate actually gates: it blocks a failing held-out check, lands a passing
one, leaves unregistered tasks byte-identical to today, and never exposes the
held-out payload to the agent's worktree.

## Outcome

flywheel's own `flywheel.toml` declares a committed `[held_out] root` that points at
a git-ignored directory, so the execute-time held-out gate is ACTIVE for this repo:
from this change forward, any self-development task with a `<root>/<task_id>.json`
registration is gated out-of-band by the orchestrator before it lands, while a task
with no registration lands exactly as it does today. The activation is proven
end-to-end through the real source-construction entry path (`build_held_out_source`
from a parsed policy, repo-root-resolved) and locked by committed tests: a config
that did not actually gate, or that gated everything on upgrade, or that leaked the
held-out payload into the worktree, fails the suite.

## Background

The program's north star is verified build-conformance: the agent never decides
whether it succeeded; an out-of-band held-out check does. 00050 built that check's
engine and 00051 made it activatable by committed config alone — but neither turned
it on HERE, and 00051's own fw-verify recorded an honest limit: the full
config-to-source-to-land/block path was verified by composition, never as a single
end-state assertion driven from a parsed policy. This spec closes both gaps at once.

The tacit, load-bearing requirement a literal agent will miss: **the value is the
committed pointer with the git-ignored payload.** A fresh git worktree materializes
only tracked files, so a `[held_out] root` pointing inside the already-ignored
`.flywheel/verification/` activates the gate for the orchestrator while every
registration and oracle stays out of every agent worktree. Committing the pointer at
a TRACKED directory would re-land the payload into the agent's view — the exact
in-repo gameability 00050 closed. The pointer is committed; the payload is not. The
two must not be conflated.

## Scope

### In scope
- A committed `[held_out] root` in flywheel's own `flywheel.toml`, pointing at a
  git-ignored directory under `.flywheel/verification/`.
- Committed tests proving, through the real `build_held_out_source` construction
  path, that a policy with `[held_out] root` set yields a gate that (a) resolves
  registrations under `<repo_root>/<root>`, (b) blocks a task whose registration's
  command grader fails and lands a task whose registration passes, (c) leaves an
  unregistered task's landing byte-identical to the no-gate baseline, and (d) keeps
  the registration and oracle files out of the agent's worktree.

### Out of scope
- Any change to the 00050 gate engine or the 00051 activation/registration code
  (`evaluate_held_out_gate`, `FilesystemHeldOutGraderSource`,
  `build_held_out_source`, `build_oracle_registration`, the `[held_out] root`
  parser). This spec configures and proves the EXISTING machinery; it does not
  re-decide or re-implement it.
- A default/catch-all held-out grader that gates every task without a per-task
  registration. The per-task `<root>/<task_id>.json` contract (00050) is unchanged;
  a catch-all is a separate future spec if the per-task path proves valuable in use.
- Authoring or registering held-out oracles for any specific past or future task,
  and gating 00057's own execution. This spec activates and proves the channel; it
  does not populate it.
- Held-out grader types beyond `command` (00050 D-5 stands); changes to where
  fw-verify authors oracles or to its discrimination/flake gates.
- flywheel-core: `Task`, `Grader`, lifecycle, harness, schema — all unchanged.

### Must not regress
- A task with no registration at the configured root lands byte-identically to today
  (the gate is opt-in per task; activation does not gate the un-registered majority).
- 00050/00051 semantics (fail-closed, exit-code verdict, NO_GATE for an unregistered
  task, before-submit-under-lease, repo-root resolution) are unchanged.
- The held-out payload (registrations, oracles) is never present in an agent
  worktree; the configured root stays git-ignored.
- The existing orchestrator, worktree, and core suites still pass; flywheel-core is
  untouched.

## Success Criteria

Each criterion grades an observable end-state — the parsed config, the landed or
blocked result, the directory the source reads, or what is present in the worktree —
never a call sequence. Tests live under `packages/flywheel-orchestrator/tests/` and
`packages/flywheel-worktree/tests/`.

1. The repo's own `flywheel.toml` parses to a non-None held-out root, and the
   directory that root names is git-ignored. [command | held-out]
   verify: a test loads the repo's `flywheel.toml` through the real policy loader and
   asserts `held_out_root` is not None; and asserts `git check-ignore` reports the
   configured root path as ignored (the payload directory is never tracked).
   defends against: committing no pointer (the gate stays dormant — the deferral this
   spec exists to end), or committing the pointer aimed at a TRACKED directory, which
   would re-land registrations and oracles into agent worktrees and re-open the
   in-repo gameability 00050 closed.

2. When a held-out source is constructed from a policy whose `[held_out] root` is a
   relative path, the source reads registrations from `<repo_root>/<root>` and from
   no other directory. [command | held-out]
   verify: a test builds the source via the real construction entry
   (`build_held_out_source`) from a policy with a relative root, then asserts a
   registration placed at `<repo_root>/<root>/<task_id>.json` is consulted while an
   identically named file under the worker's cwd or a sandbox path is not.
   defends against: resolving the root against the worker's cwd or a sandbox path so
   registrations silently miss and tasks land ungated — a config that looks active
   but gates nothing.

3. While the gate is built from `[held_out] root` config, a task whose
   `<root>/<task_id>.json` registration's command grader fails against the committed
   tree does not land (no merge/PR effect, worktree parked); a task whose
   registration's command grader passes reaches the same landed end-state as a
   no-held-out baseline. [command | held-out]
   verify: an orchestrator/worktree test builds the source from a parsed policy via
   `build_held_out_source`, drives one task whose registration (produced by the
   shipped `write_oracle_registration`) passes against the committed tree and one
   whose registration fails, and asserts the pass-task lands identically to a
   no-held-out baseline while the fail-task is blocked and parked.
   defends against: an active config whose gate verdict is evaluated but ignored at
   landing, or a registration the config-built source cannot consume — either lands
   every gated run ungated or fails closed on all of them.

4. While `[held_out] root` is configured, a task with no registration file at the
   root lands byte-identically to the no-held-out baseline. [command | held-out]
   (must-not-regress)
   verify: a test with the gate active and no `<task_id>.json` for the driven task
   asserts the task reaches the same landed end-state as a no-held-out baseline (the
   NO_GATE path; no block, no park).
   defends against: activation that changes landing behavior for the overwhelming
   majority of tasks that carry no registration — i.e. flipping the live repo to
   gate-everything on upgrade.

5. While the gate is active with a registration and its oracle file on disk at the
   configured root, neither the registration file nor the oracle file is present
   anywhere under the agent's worktree during the task's run. [command | held-out]
   verify: a test with a registration + oracle at the configured root drives a task
   and asserts the task's worktree contains neither file (the committed-pointer /
   git-ignored-payload property).
   defends against: activation that copies or materializes the registration or oracle
   into the worktree, exposing the held-out check to the agent and making "held-out"
   a lie.

6. The orchestrator, worktree, and core test suites still pass after the change, and
   no file under `packages/flywheel-core/src/flywheel_core/` is modified.
   [command | held-out] (verification-surface)
   verify: `uv run pytest packages/flywheel-orchestrator/tests/`,
   `uv run pytest packages/flywheel-worktree/tests/`, and
   `uv run pytest packages/flywheel-core/tests/` all pass; `git diff` touches no file
   under `packages/flywheel-core/src/flywheel_core/`.
   defends against: satisfying a new criterion by weakening or deleting an existing
   gating/landing/policy test, or pushing activation concerns into core.

Verification surface: this feature turns ON a grading-and-landing gate in the live
repo — it IS a verification surface. Definition of Done (inherited by every task, all
held-out where possible): the existing orchestrator + worktree + core suites still
pass (#6); no flywheel-core change (#6); the gate stays NO_GATE for unregistered
tasks so landing is unchanged for them (#4); activation never places the held-out
payload in the worktree (#5). No grading assertion may be relaxed, skipped, or
deleted; a removed assertion with no equal-or-stronger replacement is a blocking
defect.

## Decomposition Hint (for /fw-plan)
- Layer **activate + resolve + safety**: satisfies #1, #2, #4. Edits flywheel's own
  `flywheel.toml` to add `[held_out] root` pointing at a git-ignored
  `.flywheel/verification/` subdirectory, and proves the pointer parses, the
  git-ignore invariant holds, the source resolves under repo root, and an
  unregistered task lands unchanged. No dependency on the end-to-end land/block path.
- Layer **end-to-end gate proof**: satisfies #3, #5. Builds the source from a parsed
  policy via `build_held_out_source` and drives tasks through the orchestrator
  landing path to assert block-on-fail / land-on-pass and worktree-absence of the
  payload. Depends on the activation layer (it reuses the configured-root semantics).

Shared invariants both layers assert against:
- The held-out root semantics (committed pointer in `flywheel.toml`, git-ignored
  payload under `.flywheel/verification/`, resolved relative to repo root via
  `build_held_out_source`, absent from the worktree) — established by the activation
  layer; the proof layer reuses it.
- The registration format (`<root>/<task_id>.json`, command grader, produced by the
  shipped `write_oracle_registration`) — the same on-disk contract
  `FilesystemHeldOutGraderSource` already reads (00050/00051, unchanged here).

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-1: Activation is a committed `[held_out] root` pointing inside `.flywheel/verification/`  (Status: Accepted)
- Context: the gate must be turned on for this repo by config alone, without
  exposing the held-out check to the agent. `.flywheel/verification/` is already
  git-ignored (the dir fw-verify authors oracles into), so a committed pointer there
  activates the gate for the orchestrator while keeping payload out of every
  worktree. 00051 D-1 already established the committed-pointer / git-ignored-payload
  shape; this spec applies it to flywheel's own repo.
- Decision: add `[held_out] root` to `flywheel.toml` pointing at a git-ignored
  subdirectory of `.flywheel/verification/`. Commit only the pointer; never the
  registrations or oracles.
- Rejected: committing the payload itself (re-lands it into the worktree); an env var
  only (not reproducible from the committed repo); pointing at a tracked directory
  (re-opens in-repo gameability). 
- Consequences: #1 pins the pointer-present + git-ignored invariant; #5 pins
  worktree-absence; #4 pins the unregistered-task default.

### D-2: Prove activation through the real construction path, not a hand-built source  (Status: Accepted)
- Context: 00051's honest limit was that the full config-to-source-to-land/block path
  was verified only by composition. The cheapest fake of "activated" is a test that
  hand-constructs a `FilesystemHeldOutGraderSource` and asserts the gate works —
  which never exercises whether the committed CONFIG actually wires one.
- Decision: the proof tests build the source from a parsed `WorkPolicy` via the real
  `build_held_out_source` entry (repo-root-resolved), so the assertion fails if the
  config-to-source wiring regresses.
- Rejected: a full `flywheel worker` subprocess drive (flaky, slow, and the
  subprocess's exit status is a weaker end-state than the landed/blocked result);
  hand-building the source (bypasses the wiring this spec exists to prove).
- Consequences: #2/#3 grade the landed/blocked end-state with the source built from
  config; a wiring regression (cwd-resolution, None-on-set) fails the suite.

### D-3: Per-task contract only; no catch-all default grader  (Status: Accepted)
- Context: a catch-all "gate every task with out-of-band CI" would make activation
  broadly useful immediately, but it changes the 00050 source's per-task-id
  semantics — a verified gate's contract.
- Decision: keep the per-task `<root>/<task_id>.json` contract unchanged; this spec
  only activates and proves it. A catch-all is deferred to its own spec.
- Rejected: extending the source with a default grader now (expands a verified
  contract beyond what activation requires; better proven in isolation).
- Consequences: #4 is the must-not-regress anchor — an unregistered task is NO_GATE
  and lands unchanged, which is only true because there is no catch-all.

## Open Questions (accepted gaps)
None. Every criterion lowers to a `command` grader over the orchestrator and
worktree suites. The configured root's exact subdirectory name, the proof tests'
file locations, and whether the land/block drive reuses an existing orchestrator
test harness are implementation choices for /fw-plan — the spec grades the
activation, resolution, gating, safety, and worktree-absence behavior, not the path.

## Next Steps
Run `/fw-plan 00057-activate-held-out-gate` to compile these criteria into flywheel
tasks and graders.
