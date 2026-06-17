# Feature: Phase-exit integration gate

## Outcome
When every task in a phase has landed, the phase does not archive (is not declared complete) until a configured phase-verify command passes against the merged phase base — the integrated, landed result, not any single task's sandbox. A failing gate leaves the phase active with an observable reason; with no gate configured, archival behaves exactly as today.

## Background
Per-task graders are scoped to one task's diff by design, so a defect that only emerges from the *combination* of landed tasks slips through every one of them. In a real batch, lifting a Postgres refusal in one task and leaving an unmigrated read path in another produced a crash that no per-task grader covered; it surfaced only when a human ran a whole-result review at the end. The tacit expectation an operator holds but never encodes: "when the phase is done, the integrated thing actually works." flywheel verifies each task at landing but has no gate on the *assembled* phase. This spec adds one: a command run against the merged base that must pass before the phase is sealed. The operator chooses what that command is — the full suite, a broader integration script, or a structural-review gate that exits non-zero on blocking findings — so the same mechanism carries both the integration-test and the structural-audit lesson without the harness pretending an LLM judgment is a hard signal it owns.

## Scope
### In scope
- A configurable phase-verify command (read from `flywheel.toml`, e.g. `[phase] verify`, into `WorkPolicy.phase_verify`, default `None`).
- Running that command against the merged phase base (in `repo_root` on the landed result) at the point a phase becomes eligible to archive.
- Gating archival on the command's exit code: exit 0 archives the phase as today; non-zero leaves the phase active and surfaces the gate failure.

### Out of scope
- Defining or shipping a default phase-verify command — the operator configures it; unset preserves today's behavior.
- Per-task grading, the submit-time re-verification, and the landing/base-resolution mechanics (unchanged; this is strictly the post-landing, pre-archive seam).
- Automating an LLM/structural audit as a harness-owned hard gate — the harness only runs a command and reads its exit code; a structural review is in scope only insofar as the operator wires it as the phase-verify command (and it exits non-zero on blocking findings).
- Retrying or auto-fixing a failed phase gate, or any new notification surface beyond the existing phase log/state.

### Must not regress
- With no `[phase] verify` configured, a completed phase archives exactly as it does today (back-compat).
- The existing phase-base recording, archival idempotency, and retention sweep behaviors are unchanged when the gate passes or is absent.
- A pre-existing `flywheel.toml` with no `[phase]` table still loads without error.
- The full existing suite still passes.

## Success Criteria
Each criterion is atomic, grades an observable end-state, and carries its grader type, visibility, the exact check, and the gaming move it forecloses.

1. While a phase-verify command is configured, when that command exits non-zero against the merged phase base, the phase shall not be archived and the gate failure shall be observable in the phase's recorded state/output. [command | held-out]
   verify: held-out pytest — drive the archive step for a phase whose tasks are all landed, with a configured phase-verify command that exits non-zero (e.g. `false` or a script asserting a condition the merged base violates); assert the phase remains in its active/unarchived location and the failure is surfaced through the worker's outcome (recorded, not only ephemeral stderr).
   defends against: logging "gate failed" to stderr but archiving anyway (the unarchived-state assertion fails); treating any configured command as a no-op.

2. While a phase-verify command is configured, when that command exits 0 against the merged base, the phase shall archive (the existing archival completes). [command | held-out]
   verify: held-out pytest — same setup with a configured command that exits 0 (e.g. `true`); assert the phase archives exactly as the no-gate path does (same end-state as today).
   defends against: a gate that blocks archival even on success (the archived-state assertion fails); a gate that never actually runs the command.

3. While a phase-verify command is configured, the command shall be executed against the merged phase base in `repo_root` (the landed integration result), not inside an individual task's sandbox worktree. [command | held-out]
   verify: held-out pytest — configure the phase-verify command to write a marker recording its working directory / a base-only fact (a file present on the merged base but absent in any task sandbox); assert the command observed the merged base in `repo_root`, proving it witnesses cross-task integration rather than one task's tree.
   defends against: running the gate in a task sandbox (where cross-task integration is invisible) — the base-only fact the command must observe cannot be seen from a single sandbox.

4. When no phase-verify command is configured, a completed phase shall archive with the exact end-state it has today. [command | visible]
   verify: pytest — the existing archive-on-completion test passes unchanged with `phase_verify=None`; the phase archives, phase-base recording and idempotency hold.
   defends against: making the gated path the default and changing behavior for operators who configured nothing — the unchanged-end-state assertion catches any drift.

### Verification-surface (Definition of Done)
5. The existing archival/phase-base/retention suite still runs and still passes after the change, with no fewer tests collected in the touched packages than before. [command | held-out] (verification-surface)
   verify: `uv run pytest packages/flywheel-worktree packages/flywheel-orchestrator` exits 0; collected count >= the pre-change baseline.
   defends against: making the gate pass by deleting or weakening the existing archival/idempotency tests.

Verification surface: the existing suite still passes (criterion 5); new behavior is proven by held-out pytest (criteria 1-3) that drives the real archive step and asserts against the archived/active phase state and the working directory the gate command observed, never stderr.

## Decomposition Hint (for /fw-plan)
Splits along two layers; chain with a prerequisite.
- Config layer (`flywheel_orchestrator._policy`: add `[phase] verify` -> `WorkPolicy.phase_verify`, default `None`, empty/non-string -> `PolicyError`, mirroring the existing optional-string policy keys; and the init scaffolding tail gains a commented `[phase]` block): satisfies the config half of #1-#4. Shares the `_INIT_POLICY_TAIL` renderer with prior specs — append a distinct `[phase]` table, do not rewrite others'.
- Gate layer (`flywheel_worktree.worker` archival path — `archive_phases` / the orchestrator `archive_completed_phases` call site): run `phase_verify` against the merged base in `repo_root` and gate archival on its exit code; satisfies #1, #2, #3, #4; depends on the config layer.
Shared invariant: `WorkPolicy.phase_verify` (key `[phase] verify`, default `None`) — the config layer sets it, the gate layer reads it; name it so neither drifts.

## Decisions Log (ADR-style: immutable, supersede — never edit)

### D-1: The gate is one operator-configured command, exit-code gated — not a harness-owned audit  (Status: Accepted)
- Context: the lesson is "the integrated phase must be verified, and a structural review caught what unit graders missed." But a structural/LLM review is a judgment, not a deterministic signal the harness can own as a hard gate.
- Decision: the harness runs ONE operator-configured command against the merged base and gates on its exit code. The operator decides what verifies a phase — the full suite, an integration script, or a review-gate command that exits non-zero on blocking findings. The harness never interprets a review verdict itself.
- Rejected: a built-in default suite command (repo-specific; would surprise operators); an automated LLM-judge gate owned by the harness (a superficial token can flip it — fw-spec forbids an LLM judge as sole authority). Consequences: the structural-audit value is available only if the operator wires a review-gate as `phase_verify`; the harness stays a deterministic exit-code gate.

### D-2: Gate runs against the merged base in repo_root, after landing, before archive  (Status: Accepted)
- Context: cross-task integration defects are visible only on the assembled result; a per-task sandbox cannot see them. flywheel already lands each task onto the phase base before this point.
- Decision: the phase-verify command runs in `repo_root` against the merged base at the moment a phase is eligible to archive (all its tasks landed), gating the archive transition.
- Rejected: running the gate per task (defeats the purpose — no integration visible); running it on a detached copy (extra machinery; the merged base in `repo_root` is the real landed result, and 00026 guarantees the operator's working tree is not what landing mutated). Consequences: the gate command observes the same merged base an operator would inspect.

### D-3: Default unset is fully back-compatible  (Status: Accepted)
- Context: existing adopters have no `[phase] verify`; their archival must not change.
- Decision: `phase_verify=None` (the default) takes the exact archival path that exists today — no gate, no behavior change.
- Rejected: defaulting the gate on (breaks every existing adopter's archival the moment they upgrade). Consequences: the gate is opt-in; criterion 4 pins the unchanged default.

## Open Questions (accepted gaps)
None. Criteria 1-4 lower to `command` graders; criteria 1-3 are held-out pytest checks that drive the real archive step and assert against archived/active phase state and the gate command's observed working directory.

## Next Steps
Run `/fw-plan 00035-FEATURE-phase-exit-integration-gate` to compile these criteria into flywheel tasks and graders.
